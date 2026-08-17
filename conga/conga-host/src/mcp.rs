//! MCP (Model Context Protocol) client — connect external MCP tool servers.
//!
//! Legacy-era stdio client (protocol version `2025-06-18`): spawns an MCP
//! server as a subprocess, runs the `initialize` handshake, discovers tools
//! via `tools/list`, and wraps each as a [`ToolDefinition`]. Tool invocation
//! sends `tools/call`, matching the response by JSON-RPC `id` while skipping
//! server-sent notifications.
//!
//! Configuration: `~/.conga/mcp.json` (or `$CONGA_MCP_CONFIG`), Claude-Desktop
//! style `{"mcpServers": {name: {command, args, env}}}`. Parallel to
//! [`crate::external_tool::ExternalToolBridge`]; both produce `Vec<ToolDefinition>`.
//!
//! ## Serialization constraint (both transports)
//!
//! Calls to ONE server are fully serialized (stdio: one connection-level
//! mutex held across request+response; HTTP: a per-client id mutex). This
//! is a deliberate simplification — MCP servers are frequently
//! single-threaded subprocesses — NOT an accident. Parallel fan-out across
//! *different* servers is unaffected.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use conga::{ContentBlock, RiskLevel, ToolDefinition, ToolError, ToolResult};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const PROTOCOL_VERSION: &str = "2025-06-18";

// ── Errors ──────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("call timeout after {0:?}")]
    Timeout(Duration),
    #[error("server returned error {code}: {message}")]
    ServerError { code: i64, message: String },
}

// ── Config ──────────────────────────────────────────────────────

/// One MCP server entry from `mcp.json`.
///
/// Two mutually-exclusive transport modes:
/// - **stdio** (default): `command` + `args` + `env` — spawns a subprocess.
/// - **Streamable HTTP**: `url` + `headers` — POSTs JSON-RPC to a remote server.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    /// stdio: command to run (mutually exclusive with `url`).
    #[serde(default)]
    pub command: Option<String>,
    /// stdio: command arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio: extra environment variables for the subprocess.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Streamable HTTP: server URL (mutually exclusive with `command`).
    #[serde(default)]
    pub url: Option<String>,
    /// Streamable HTTP: extra HTTP headers (e.g. `Authorization: Bearer ...`).
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

impl McpServerConfig {
    /// True if this entry configures a Streamable HTTP server.
    fn is_http(&self) -> bool {
        self.url.is_some()
    }
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServerConfig>,
}

/// Read MCP config: `$CONGA_MCP_CONFIG` path, else `~/.conga/mcp.json`.
/// Missing file → empty vec. Bad JSON → error.
pub fn load_config() -> Result<Vec<(String, McpServerConfig)>, McpError> {
    let path = mcp_config_path();
    load_config_from(&path)
}

/// Same as [`load_config`] but from an explicit path (tests).
pub fn load_config_from(
    path: &std::path::Path,
) -> Result<Vec<(String, McpServerConfig)>, McpError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(McpError::Io(e)),
    };
    let cfg: ConfigFile = serde_json::from_str(&text)?;
    Ok(cfg.mcp_servers.into_iter().collect())
}

fn mcp_config_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("CONGA_MCP_CONFIG") {
        return std::path::PathBuf::from(p);
    }
    conga::storage::config_dir().join("mcp.json")
}

// ── JSON-RPC wire types ─────────────────────────────────────────

/// A request we send (has `id` + `method` + optional `params`).
async fn write_request(
    stdin: &mut ChildStdin,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> Result<(), McpError> {
    let msg = serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": method, "params": params
    });
    let line = serde_json::to_string(&msg)?;
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

/// A notification we send (no `id`).
async fn write_notification(stdin: &mut ChildStdin, method: &str) -> Result<(), McpError> {
    let msg = serde_json::json!({"jsonrpc": "2.0", "method": method});
    let line = serde_json::to_string(&msg)?;
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

/// One line read from the server's stdout — a response, error, or notification.
#[derive(Debug)]
enum IncomingMessage {
    /// A successful response for our request `id`.
    Response { result: serde_json::Value },
    /// An error response for our request `id`.
    Error { code: i64, message: String },
    /// Anything else: notification (no id), or a response/error whose id
    /// doesn't match what we're waiting for. Skipped by the caller.
    Other,
}

fn parse_incoming(line: &str, expected_id: u64) -> IncomingMessage {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return IncomingMessage::Other,
    };
    // Notification: no "id" field.
    let Some(id_val) = v.get("id") else {
        return IncomingMessage::Other;
    };
    let id = id_val.as_u64().unwrap_or(u64::MAX);
    if id != expected_id {
        return IncomingMessage::Other;
    }
    if let Some(err) = v.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .to_string();
        IncomingMessage::Error { code, message }
    } else if v.get("result").is_some() {
        IncomingMessage::Response {
            result: v["result"].clone(),
        }
    } else {
        IncomingMessage::Other
    }
}

// ── MCP tool/content types (from server) ────────────────────────

#[derive(Debug, Deserialize)]
struct McpTool {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    #[allow(dead_code)]
    annotations: serde_json::Value,
    #[serde(rename = "inputSchema")]
    input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ToolsListResult {
    #[serde(default, rename = "tools")]
    list: Vec<McpTool>,
}

#[derive(Debug, Deserialize)]
struct CallResult {
    #[serde(default)]
    content: Vec<McpContent>,
    #[serde(default)]
    is_error: bool,
}

#[derive(Debug, Deserialize)]
struct McpContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default, rename = "mimeType")]
    mime_type: Option<String>,
}

/// Map MCP content items to conga's `ContentBlock`s.
fn content_to_blocks(items: &[McpContent]) -> Vec<ContentBlock> {
    let blocks: Vec<ContentBlock> = items
        .iter()
        .filter_map(|c| match c.kind.as_str() {
            "text" => c.text.clone().map(ContentBlock::text),
            // Vision is not on conga's wire: providers would silently drop
            // image blocks. Say so instead of constructing a block the model
            // never sees.
            "image" => Some(ContentBlock::text(format!(
                "[image content omitted: {}, {} bytes of base64]",
                c.mime_type.as_deref().unwrap_or("unknown mime"),
                c.data.as_deref().map(str::len).unwrap_or(0),
            ))),
            _ => Some(ContentBlock::text(format!("{c:?}"))),
        })
        .collect();
    if blocks.is_empty() {
        vec![ContentBlock::text(String::new())]
    } else {
        blocks
    }
}

// ── Bridge ──────────────────────────────────────────────────────

struct McpBridgeInner {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

/// One long-lived MCP server process. Each tool's `execute` closure holds an
/// `Arc<McpBridge>`; dropping all tools drops the bridge → `kill_on_drop`
/// reaps the subprocess.
pub struct McpBridge {
    inner: Mutex<McpBridgeInner>,
    timeout: Duration,
    server_name: String,
}

impl McpBridge {
    /// Spawn a server, run the handshake, discover tools.
    pub async fn spawn(
        name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<(Arc<Self>, Vec<ToolDefinition>), McpError> {
        let mut child = Command::new(command)
            .args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Protocol("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Protocol("no stdout".into()))?;

        let inner = McpBridgeInner {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };

        let bridge = Arc::new(Self {
            inner: Mutex::new(inner), // temporarily moves; we re-extract below
            timeout,
            server_name: name.to_string(),
        });

        // Handshake — IDs 1 (initialize) and 2 (tools/list).
        {
            let mut guard = bridge.inner.lock().await;
            // 1. initialize
            write_request(
                &mut guard.stdin,
                1,
                "initialize",
                serde_json::json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientInfo": {
                        "name": "conga",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {},
                }),
            )
            .await?;
            let _init_result = bridge.recv_response(&mut guard, 1).await?;

            // 2. initialized notification
            write_notification(&mut guard.stdin, "notifications/initialized").await?;

            // 3. tools/list
            write_request(&mut guard.stdin, 2, "tools/list", serde_json::json!({})).await?;
        }

        let tools_list: ToolsListResult = bridge
            .call_typed(2, |result| {
                serde_json::from_value(result)
                    .map_err(|e| McpError::Protocol(format!("tools/list parse error: {e}")))
            })
            .await?;

        let tools = tools_list
            .list
            .into_iter()
            .map(|t| bridge.tool_definition(t))
            .collect();
        Ok((bridge, tools))
    }

    /// Send a request and wait for its response, deserializing the result.
    async fn call_typed<T, F>(&self, id: u64, map: F) -> Result<T, McpError>
    where
        F: FnOnce(serde_json::Value) -> Result<T, McpError>,
    {
        let mut guard = self.inner.lock().await;
        let result = self.recv_response(&mut guard, id).await?;
        map(result)
    }

    /// Read lines until we find the response for `id`, skipping notifications
    /// and unmatched messages. Borrows `inner` via `guard` so we hold the lock
    /// for the entire read window.
    async fn recv_response(
        &self,
        guard: &mut McpBridgeInner,
        id: u64,
    ) -> Result<serde_json::Value, McpError> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let mut line = String::new();
            let read_fut = guard.stdout.read_line(&mut line);
            let n = match tokio::time::timeout_at(deadline, read_fut).await {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(McpError::Io(e)),
                Err(_) => return Err(McpError::Timeout(self.timeout)),
            };
            if n == 0 {
                return Err(McpError::Protocol("server closed stdout".into()));
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match parse_incoming(line, id) {
                IncomingMessage::Response { result, .. } => return Ok(result),
                IncomingMessage::Error { code, message, .. } => {
                    return Err(McpError::ServerError { code, message });
                }
                IncomingMessage::Other => continue,
            }
        }
    }

    /// Invoke an MCP tool by its original (un-prefixed) name.
    async fn call(
        &self,
        original_name: &str,
        args: &serde_json::Value,
    ) -> Result<CallResult, McpError> {
        let mut guard = self.inner.lock().await;
        let id = guard.next_id;
        guard.next_id += 1;
        write_request(
            &mut guard.stdin,
            id,
            "tools/call",
            serde_json::json!({"name": original_name, "arguments": args}),
        )
        .await?;
        let result = self.recv_response(&mut guard, id).await?;
        serde_json::from_value(result)
            .map_err(|e| McpError::Protocol(format!("tools/call parse error: {e}")))
    }

    /// Wrap one MCP tool as a conga [`ToolDefinition`] — via the shared
    /// [`mcp_tool_definition`] wrapper (same naming/risk/dispatch as the
    /// HTTP transport).
    fn tool_definition(self: &Arc<Self>, t: McpTool) -> ToolDefinition {
        let bridge = Arc::clone(self);
        mcp_tool_definition(
            &self.server_name,
            t,
            Arc::new(move |name, args| {
                let bridge = Arc::clone(&bridge);
                Box::pin(async move { bridge.call(&name, &args).await })
            }),
        )
    }
}

// ── Streamable HTTP transport ───────────────────────────────────

/// Proxy for remote MCP traffic: the tool-proxy system first (runtime
/// override > `CONGA_TOOL_PROXY`), then the legacy LLM-proxy env chain for
/// backward compatibility. Direct connection when none is set.
fn pick_mcp_proxy(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> Option<String> {
    if let Some(p) = crate::proxy::tool_proxy() {
        return Some(p);
    }
    ["CONGA_LLM_PROXY", "HTTPS_PROXY", "https_proxy"]
        .iter()
        .find_map(|k| lookup(k).ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
}

/// Session header for the Streamable HTTP transport: stateful servers
/// assign one at `initialize` and expect it echoed on every later request;
/// stateless servers never send it.
const SESSION_ID_HEADER: &str = "mcp-session-id";

/// Incremental SSE decoder for the Streamable HTTP transport.
///
/// Feed raw body chunks as they arrive; complete events (terminated by a
/// blank line) come back as payloads with their `data:` lines joined by
/// `\n` per the SSE spec. Handles LF / CRLF / CR line endings — including a
/// CRLF split across two chunks — `:` comments, and the single-space strip
/// after the field colon.
struct SseDecoder {
    /// Bytes of a line not yet terminated (may split mid-UTF-8).
    buf: Vec<u8>,
    /// `data:` lines of the event in progress.
    data: Vec<String>,
}

impl SseDecoder {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            data: Vec::new(),
        }
    }

    /// Feed one body chunk; returns every event completed by a blank line.
    fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n' || b == b'\r') {
            // A CR at the very end of the buffer may be half of a CRLF whose
            // LF lands in the next chunk — wait for it.
            if self.buf[i] == b'\r' && i + 1 == self.buf.len() {
                break;
            }
            let line = String::from_utf8_lossy(&self.buf[..i]).into_owned();
            let mut consumed = i + 1;
            if self.buf[i] == b'\r' && self.buf.get(i + 1) == Some(&b'\n') {
                consumed += 1;
            }
            self.buf.drain(..consumed);
            if self.process_line(&line) {
                // Blank line → dispatch: no data lines, no event.
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                }
                self.data.clear();
            }
        }
        events
    }

    /// Stream ended: treat the trailing unterminated line (if any) as
    /// complete, then dispatch the pending event one final time.
    fn flush(&mut self) -> Vec<String> {
        let mut line = &self.buf[..];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let line = String::from_utf8_lossy(line).into_owned();
        self.buf.clear();
        self.process_line(&line);
        if self.data.is_empty() {
            Vec::new()
        } else {
            vec![std::mem::take(&mut self.data).join("\n")]
        }
    }

    /// Process one complete line into the pending event; `true` on a blank
    /// line (the SSE event-dispatch point).
    fn process_line(&mut self, line: &str) -> bool {
        if line.is_empty() {
            return true;
        }
        if line.starts_with(':') {
            return false; // comment
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        if field == "data" {
            self.data.push(value.to_string());
        }
        false
    }
}

/// Try one dispatched SSE event payload as the JSON-RPC response for `id`:
/// `None` = not ours (notification or different id), `Some(Ok)` = result,
/// `Some(Err)` = a JSON-RPC error addressed to us.
fn sse_payload(payload: &str, id: u64) -> Option<Result<serde_json::Value, McpError>> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    match parse_jsonrpc_value(&v, id) {
        Parsed::Result(result) => Some(Ok(result)),
        Parsed::Error { code, message } => Some(Err(McpError::ServerError { code, message })),
        Parsed::Other => None,
    }
}

/// One long-lived MCP HTTP client. Each tool's `execute` closure holds an
/// `Arc<McpHttpClient>`; calls are POSTs to the server URL, echoing the
/// server-assigned session id (if any) on every request after `initialize`.
pub struct McpHttpClient {
    client: reqwest::Client,
    url: String,
    headers: HashMap<String, String>,
    timeout: Duration,
    server_name: String,
    next_id: tokio::sync::Mutex<u64>,
    /// Session id assigned by the server at `initialize`, if any.
    session_id: Mutex<Option<String>>,
}

impl McpHttpClient {
    /// Connect to a Streamable HTTP MCP server, run the handshake, discover tools.
    pub async fn connect(
        name: &str,
        url: &str,
        headers: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<(Arc<Self>, Vec<ToolDefinition>), McpError> {
        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(false);
        // Proxy support: tool-proxy system (override > CONGA_TOOL_PROXY)
        // first, then the legacy CONGA_LLM_PROXY / HTTPS_PROXY env chain.
        if let Some(proxy_url) = pick_mcp_proxy(&|k| std::env::var(k)) {
            if let Ok(proxy) = reqwest::Proxy::all(&proxy_url) {
                builder = builder.proxy(proxy);
            }
        }
        let client = builder
            .build()
            .map_err(|e| McpError::Protocol(format!("http client build error: {e}")))?;

        let http = Arc::new(Self {
            client,
            url: url.to_string(),
            headers: headers.clone(),
            timeout,
            server_name: name.to_string(),
            next_id: tokio::sync::Mutex::new(1),
            session_id: Mutex::new(None),
        });

        // Handshake — same sequence as stdio, just over HTTP POST.
        // 1. initialize
        let _init = http
            .request(
                1,
                "initialize",
                serde_json::json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientInfo": {
                        "name": "conga",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {},
                }),
            )
            .await?;

        // 2. notifications/initialized (no response expected, but we POST it)
        http.notification("notifications/initialized").await?;

        // 3. tools/list
        let tools_result = http.request(2, "tools/list", serde_json::json!({})).await?;
        let tools_list: ToolsListResult = serde_json::from_value(tools_result)
            .map_err(|e| McpError::Protocol(format!("tools/list parse error: {e}")))?;

        let tools = tools_list
            .list
            .into_iter()
            .map(|t| http.tool_definition(t))
            .collect();
        Ok((http, tools))
    }

    /// POST a JSON-RPC request and parse the response (single JSON or SSE stream).
    async fn request(
        &self,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let deadline = tokio::time::Instant::now() + self.timeout;
        let send_fut = self.post_json(&body);
        let resp = tokio::time::timeout_at(deadline, send_fut)
            .await
            .map_err(|_| McpError::Timeout(self.timeout))??;

        // Stateful servers assign a session id (at initialize) and expect it
        // on every later request; stateless servers never send one.
        self.capture_session_id(resp.headers()).await;

        // Parse based on Content-Type.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            // SSE: consume the stream incrementally and return on the event
            // matching our id — servers may keep the stream open.
            self.read_sse(resp, id, deadline).await
        } else {
            // Single JSON response.
            let v: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| McpError::Protocol(format!("http response json parse error: {e}")))?;
            match parse_jsonrpc_value(&v, id) {
                Parsed::Result(result) => Ok(result),
                Parsed::Error { code, message } => Err(McpError::ServerError { code, message }),
                Parsed::Other => Err(McpError::Protocol(format!(
                    "http response has no result/error for id {id}: {v}"
                ))),
            }
        }
    }

    /// POST a notification (no id, no response expected).
    async fn notification(&self, method: &str) -> Result<(), McpError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        self.post_json(&body).await?;
        Ok(())
    }

    /// Send a POST and return the raw response.
    async fn post_json(&self, body: &serde_json::Value) -> Result<reqwest::Response, McpError> {
        let mut req = self
            .client
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            );
        // Echo the server-assigned session id once captured at initialize.
        if let Some(session_id) = self.session_id.lock().await.clone() {
            req = req.header(SESSION_ID_HEADER, session_id);
        }
        for (k, v) in &self.headers {
            if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes()) {
                if let Ok(val) = reqwest::header::HeaderValue::from_str(v) {
                    req = req.header(name, val);
                }
            }
        }
        req.json(body)
            .send()
            .await
            .map_err(|e| McpError::Protocol(format!("http post error: {e}")))
    }

    /// Record the server-assigned session id from a response header, if the
    /// server sent one (empty values are ignored).
    async fn capture_session_id(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(v) = headers
            .get(SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            *self.session_id.lock().await = Some(v.to_string());
        }
    }

    /// Consume an SSE response body incrementally: return as soon as the
    /// JSON-RPC message matching `id` arrives, dropping the rest of the
    /// stream — Streamable-HTTP servers may keep it open indefinitely, so
    /// waiting for EOF (or buffering the whole body) can hang.
    async fn read_sse(
        &self,
        resp: reqwest::Response,
        id: u64,
        deadline: tokio::time::Instant,
    ) -> Result<serde_json::Value, McpError> {
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::new();
        loop {
            let chunk = match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(Some(Err(e))) => {
                    return Err(McpError::Protocol(format!("http body read error: {e}")))
                }
                Ok(None) => {
                    // Stream ended — dispatch any trailing unterminated event.
                    for payload in decoder.flush() {
                        if let Some(res) = sse_payload(&payload, id) {
                            return res;
                        }
                    }
                    break;
                }
                Err(_) => return Err(McpError::Timeout(self.timeout)),
            };
            for payload in decoder.feed(&chunk) {
                if let Some(res) = sse_payload(&payload, id) {
                    return res; // matched — stop reading, drop the stream
                }
            }
        }
        Err(McpError::Protocol(format!(
            "SSE stream ended without a response for id {id}"
        )))
    }

    /// Invoke an MCP tool by its original (un-prefixed) name.
    async fn call(
        &self,
        original_name: &str,
        args: &serde_json::Value,
    ) -> Result<CallResult, McpError> {
        let id = {
            let mut guard = self.next_id.lock().await;
            let id = *guard;
            *guard += 1;
            id
        };
        let result = self
            .request(
                id,
                "tools/call",
                serde_json::json!({"name": original_name, "arguments": args}),
            )
            .await?;
        serde_json::from_value(result)
            .map_err(|e| McpError::Protocol(format!("tools/call parse error: {e}")))
    }

    /// Wrap one MCP tool as a conga [`ToolDefinition`] — via the shared
    /// [`mcp_tool_definition`] wrapper (same naming/risk/dispatch as the
    /// stdio transport).
    fn tool_definition(self: &Arc<Self>, t: McpTool) -> ToolDefinition {
        let client = Arc::clone(self);
        mcp_tool_definition(
            &self.server_name,
            t,
            Arc::new(move |name, args| {
                let client = Arc::clone(&client);
                Box::pin(async move { client.call(&name, &args).await })
            }),
        )
    }
}

// ── Shared tool wrapper (both transports) ─────────────────────

/// One `tools/call` dispatch, transport-agnostic: `(original_name, args)`
/// → the server's `CallResult`.
type McpCallFn = Arc<
    dyn Fn(
            String,
            serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<CallResult, McpError>> + Send>>
        + Send
        + Sync,
>;

/// Wrap one MCP tool as a conga [`ToolDefinition`], shared by the stdio and
/// HTTP transports: name prefixed `mcp__<server>__<tool>` (server tool
/// names can collide with built-ins or each other), risk High (an external
/// server is unvetted code), execution dispatched through `call`.
fn mcp_tool_definition(server_name: &str, t: McpTool, call: McpCallFn) -> ToolDefinition {
    let original_name = t.name.clone();
    let prefixed_name = format!("mcp__{server_name}__{}", t.name);
    let label = format!("{}/{}", server_name, t.title.unwrap_or(t.name));
    ToolDefinition {
        name: prefixed_name,
        label,
        description: t.description,
        parameters: t.input_schema,
        risk: RiskLevel::High,
        execute: Arc::new(move |ctx| {
            let call = Arc::clone(&call);
            let original_name = original_name.clone();
            Box::pin(async move {
                if ctx.aborted() {
                    return Ok(ToolResult::error("aborted"));
                }
                match call(original_name, ctx.args).await {
                    Ok(resp) => Ok(ToolResult {
                        content: content_to_blocks(&resp.content),
                        details: serde_json::Value::Null,
                        is_error: resp.is_error,
                    }),
                    Err(e) => Err(ToolError::Message(e.to_string())),
                }
            })
        }),
    }
}

/// Parsed JSON-RPC message (shared by stdio and HTTP paths).
enum Parsed {
    Result(serde_json::Value),
    Error { code: i64, message: String },
    Other,
}

/// Parse a JSON-RPC value, matching by `id`.
fn parse_jsonrpc_value(v: &serde_json::Value, expected_id: u64) -> Parsed {
    let Some(id_val) = v.get("id") else {
        return Parsed::Other; // notification
    };
    let id = id_val.as_u64().unwrap_or(u64::MAX);
    if id != expected_id {
        return Parsed::Other;
    }
    if let Some(err) = v.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .to_string();
        Parsed::Error { code, message }
    } else if v.get("result").is_some() {
        Parsed::Result(v["result"].clone())
    } else {
        Parsed::Other
    }
}

// ── Top-level loader ────────────────────────────────────────────

/// Spawn every configured MCP server; collect tools. Per-server failures are
/// logged and skipped (matching `load_external_tools`'s tolerance).
///
/// Dispatches by transport: `url` → Streamable HTTP, `command` → stdio.
pub async fn load_all_mcp() -> Vec<ToolDefinition> {
    let configs = match load_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("mcp config load failed: {e}");
            return Vec::new();
        }
    };
    let timeout = mcp_call_timeout();
    let mut tools = Vec::new();
    for (name, cfg) in configs {
        let defs: Vec<ToolDefinition> = if cfg.is_http() {
            let url = cfg.url.as_ref().expect("is_http guarantees url");
            match McpHttpClient::connect(&name, url, &cfg.headers, timeout).await {
                Ok((_, defs)) => defs,
                Err(e) => {
                    tracing::warn!("mcp {name} load failed: {e}");
                    continue;
                }
            }
        } else {
            let command = cfg.command.as_deref().unwrap_or("");
            match McpBridge::spawn(&name, command, &cfg.args, &cfg.env, timeout).await {
                Ok((_, defs)) => defs,
                Err(e) => {
                    tracing::warn!("mcp {name} load failed: {e}");
                    continue;
                }
            }
        };
        if !defs.is_empty() {
            tracing::info!("mcp {name}: {} tools", defs.len());
        }
        tools.extend(defs);
    }
    tools
}

/// Read the MCP call timeout from `CONGA_MCP_CALL_TIMEOUT_S` (default 60s).
fn mcp_call_timeout() -> Duration {
    std::env::var("CONGA_MCP_CALL_TIMEOUT_S")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pure-function tests (no subprocess / network) ────────────

    #[test]
    fn parse_incoming_matches_response_by_id() {
        let line = r#"{"jsonrpc":"2.0","id":5,"result":{"ok":true}}"#;
        match parse_incoming(line, 5) {
            IncomingMessage::Response { result, .. } => {
                assert_eq!(result["ok"], true);
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn parse_incoming_skips_unmatched_id() {
        let line = r#"{"jsonrpc":"2.0","id":3,"result":{}}"#;
        assert!(matches!(parse_incoming(line, 5), IncomingMessage::Other));
    }

    #[test]
    fn parse_incoming_treats_notification_as_other() {
        let line = r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#;
        assert!(matches!(parse_incoming(line, 5), IncomingMessage::Other));
    }

    #[test]
    fn parse_incoming_parses_error() {
        let line = r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message"|"not found"}}"#;
        // malformed JSON (| instead of :) → Other
        assert!(matches!(parse_incoming(line, 7), IncomingMessage::Other));

        let line = r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"not found"}}"#;
        match parse_incoming(line, 7) {
            IncomingMessage::Error { code, message, .. } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "not found");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn content_maps_text_and_image() {
        let items = vec![
            McpContent {
                kind: "text".into(),
                text: Some("hello".into()),
                data: None,
                mime_type: None,
            },
            McpContent {
                kind: "image".into(),
                text: None,
                data: Some("base64data".into()),
                mime_type: Some("image/png".into()),
            },
        ];
        let blocks = content_to_blocks(&items);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "hello"));
        match &blocks[1] {
            ContentBlock::Text { text } => {
                assert_eq!(
                    text,
                    "[image content omitted: image/png, 10 bytes of base64]"
                );
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn content_empty_vec_gives_one_empty_text() {
        let blocks = content_to_blocks(&[]);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text.is_empty()));
    }

    #[test]
    fn content_unknown_type_degrades_to_text() {
        let items = vec![McpContent {
            kind: "audio".into(),
            text: None,
            data: Some("audio-data".into()),
            mime_type: Some("audio/wav".into()),
        }];
        let blocks = content_to_blocks(&items);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text { .. }));
    }

    #[test]
    fn config_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.json");
        let cfg = load_config_from(&path).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn config_parses_mcp_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{
                "mcpServers": {
                    "github": {
                        "command": "npx",
                        "args": ["-y", "server-github"],
                        "env": { "TOKEN": "secret" }
                    },
                    "fs": {
                        "command": "npx",
                        "args": ["-y", "server-fs"]
                    }
                }
            }"#,
        )
        .unwrap();
        let cfg = load_config_from(&path).unwrap();
        assert_eq!(cfg.len(), 2);
        let github = cfg
            .iter()
            .find(|(n, _)| n == "github")
            .map(|(_, c)| c)
            .unwrap();
        assert_eq!(github.command.as_deref(), Some("npx"));
        assert_eq!(github.args, vec!["-y", "server-github"]);
        assert_eq!(github.env.get("TOKEN").unwrap(), "secret");
    }

    #[test]
    fn config_parses_http_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{
                "mcpServers": {
                    "remote": {
                        "url": "https://mcp.example.dev/mcp",
                        "headers": { "Authorization": "Bearer tok123" }
                    },
                    "local": {
                        "command": "npx",
                        "args": ["-y", "server-fs"]
                    }
                }
            }"#,
        )
        .unwrap();
        let cfg = load_config_from(&path).unwrap();
        assert_eq!(cfg.len(), 2);
        let remote = cfg
            .iter()
            .find(|(n, _)| n == "remote")
            .map(|(_, c)| c)
            .unwrap();
        assert!(remote.is_http());
        assert_eq!(remote.url.as_deref(), Some("https://mcp.example.dev/mcp"));
        assert_eq!(
            remote.headers.get("Authorization").unwrap(),
            "Bearer tok123"
        );
        assert!(remote.command.is_none());

        let local = cfg
            .iter()
            .find(|(n, _)| n == "local")
            .map(|(_, c)| c)
            .unwrap();
        assert!(!local.is_http());
        assert_eq!(local.command.as_deref(), Some("npx"));
        assert!(local.url.is_none());
    }

    // ── HTTP transport pure-function tests ─────────────────────

    #[test]
    fn parse_jsonrpc_value_matches_result_by_id() {
        let v = serde_json::json!({"jsonrpc":"2.0","id":3,"result":{"tools":[]}});
        match parse_jsonrpc_value(&v, 3) {
            Parsed::Result(r) => assert!(r["tools"].is_array()),
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn parse_jsonrpc_value_matches_error_by_id() {
        let v =
            serde_json::json!({"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"nope"}});
        match parse_jsonrpc_value(&v, 5) {
            Parsed::Error { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "nope");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn parse_jsonrpc_value_skips_unmatched_id_and_notifications() {
        // Wrong id
        let v = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}});
        assert!(matches!(parse_jsonrpc_value(&v, 99), Parsed::Other));

        // Notification (no id)
        let v = serde_json::json!({"jsonrpc":"2.0","method":"notifications/progress"});
        assert!(matches!(parse_jsonrpc_value(&v, 1), Parsed::Other));
    }

    #[test]
    fn sse_response_extracts_matching_data_line() {
        // Simulate an SSE body: a notification event, then the matching response.
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"isError\":false}}\n\n";
        let mut decoder = SseDecoder::new();
        let v = decoder
            .feed(sse.as_bytes())
            .into_iter()
            .find_map(|payload| sse_payload(&payload, 7))
            .expect("should find matching response")
            .expect("matching response should be a result");
        assert_eq!(v["content"][0]["text"], "hi");
        assert_eq!(v["isError"], false);
    }

    #[test]
    fn sse_response_missing_match_returns_error() {
        let sse = "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n";
        let mut decoder = SseDecoder::new();
        assert!(
            decoder
                .feed(sse.as_bytes())
                .into_iter()
                .all(|payload| sse_payload(&payload, 7).is_none()),
            "should not find a match for id=7"
        );
    }

    #[test]
    fn sse_feed_returns_match_before_stream_drains() {
        // The decoder hands back events as chunks arrive, so the caller can
        // return on the matching id even though more data follows. Feed one
        // byte at a time — the worst-case chunk split.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n",
        );
        let mut decoder = SseDecoder::new();
        let mut result = None;
        'outer: for byte in body.bytes() {
            for payload in decoder.feed(&[byte]) {
                if let Some(res) = sse_payload(&payload, 7) {
                    result = Some(res);
                    break 'outer;
                }
            }
        }
        let v = result
            .expect("match must land mid-stream")
            .expect("match should be a result");
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn sse_multiline_data_fields_join_with_newline() {
        // Per the SSE spec, consecutive `data:` lines of one event join
        // with a single `\n` — so a JSON-RPC response pretty-printed across
        // data lines must still parse.
        let mut decoder = SseDecoder::new();
        assert_eq!(
            decoder.feed(b"data: first\ndata: second\n\n"),
            vec!["first\nsecond".to_string()]
        );

        let stream = "data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":4,\"result\":{\"n\":1}}\n\n";
        let mut decoder = SseDecoder::new();
        let v = decoder
            .feed(stream.as_bytes())
            .into_iter()
            .find_map(|payload| sse_payload(&payload, 4))
            .expect("joined data must parse as the response")
            .expect("result");
        assert_eq!(v["n"], 1);
    }

    #[test]
    fn sse_decoder_handles_terminators_comments_and_split_chunks() {
        // Comment lines are ignored; one space after the colon is stripped.
        let mut d = SseDecoder::new();
        assert_eq!(
            d.feed(b": keep-alive\ndata: spaced\n\n"),
            vec!["spaced".to_string()]
        );

        // CRLF split across chunks: a trailing CR waits for the next byte.
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: crlf\r").is_empty());
        assert_eq!(d.feed(b"\n\r\n"), vec!["crlf".to_string()]);

        // Bare CR line endings also terminate lines; a trailing lone CR
        // could still be half of a CRLF, so it defers to the next chunk or
        // flush.
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: cr\rdata: d\r\r").is_empty());
        assert_eq!(d.flush(), vec!["cr\nd".to_string()]);

        // EOF flushes a trailing unterminated data line as an event.
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: tail").is_empty());
        assert_eq!(d.flush(), vec!["tail".to_string()]);
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: done\n").is_empty());
        assert_eq!(d.flush(), vec!["done".to_string()]);
        // ... including when it ends in a lone (deferred) CR.
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: crtail\r").is_empty());
        assert_eq!(d.flush(), vec!["crtail".to_string()]);
    }

    // ── Streamable HTTP transport: local raw-HTTP MCP server ─────

    /// One request as observed by the fake MCP HTTP server.
    #[derive(Debug)]
    struct SeenRequest {
        method: String,
        session_id: Option<String>,
    }

    /// Read one HTTP request (head + Content-Length body) from `sock`.
    /// Returns the head text and the JSON body, or `None` on EOF.
    async fn read_http_request(
        sock: &mut tokio::net::TcpStream,
        buf: &mut Vec<u8>,
    ) -> Option<(String, serde_json::Value)> {
        use tokio::io::AsyncReadExt;

        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            let mut chunk = [0u8; 4096];
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let len: usize = head
            .to_ascii_lowercase()
            .lines()
            .find(|l| l.starts_with("content-length:"))
            .and_then(|l| l.split_once(':'))
            .and_then(|(_, v)| v.trim().parse().ok())
            .unwrap_or(0);
        let total = head_end + 4 + len;
        while buf.len() < total {
            let mut chunk = [0u8; 4096];
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = serde_json::from_slice(&buf[head_end + 4..total]).ok()?;
        buf.drain(..total);
        Some((head, body))
    }

    /// Write one HTTP/1.1 chunked-encoding frame (`len\r\ndata\r\n`).
    async fn write_chunk(sock: &mut tokio::net::TcpStream, event: &[u8]) {
        use tokio::io::AsyncWriteExt;

        let mut frame = format!("{:x}\r\n", event.len()).into_bytes();
        frame.extend_from_slice(event);
        frame.extend_from_slice(b"\r\n");
        sock.write_all(&frame).await.unwrap();
    }

    /// Extract the Mcp-Session-Id request header, if present.
    fn session_id_of(head: &str) -> Option<String> {
        head.lines().find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("mcp-session-id")
                .then(|| value.trim().to_string())
        })
    }

    /// Fake Streamable-HTTP MCP server on loopback: `initialize` →
    /// `notifications/initialized` → `tools/list` over plain JSON, then one
    /// `tools/call` answered as a slow SSE stream (a notification event,
    /// then the matching response split over two `data:` lines, then more
    /// events — and the stream stays open until the client hangs up).
    /// `session` = the Mcp-Session-Id the initialize response carries
    /// (`None` = stateless server). Handles exactly 4 requests.
    async fn fake_mcp_http_server(
        session: Option<&'static str>,
    ) -> (String, tokio::task::JoinHandle<Vec<SeenRequest>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..4 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let Some((head, body)) = read_http_request(&mut sock, &mut buf).await else {
                    continue;
                };
                let method = body
                    .get("method")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                seen.push(SeenRequest {
                    session_id: session_id_of(&head),
                    method: method.clone(),
                });
                match method.as_str() {
                    "initialize" => {
                        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1.0"}}}"#;
                        let session_line = session
                            .map(|s| format!("{SESSION_ID_HEADER}: {s}\r\n"))
                            .unwrap_or_default();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session_line}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        sock.write_all(resp.as_bytes()).await.unwrap();
                    }
                    "notifications/initialized" => {
                        sock.write_all(
                            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    }
                    "tools/list" => {
                        let body = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo back the arguments","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        sock.write_all(resp.as_bytes()).await.unwrap();
                    }
                    "tools/call" => {
                        // SSE stream over chunked framing, written in
                        // pieces: notification first, then the response —
                        // echoing the REQUEST's id, split across two `data:`
                        // lines (multi-line data per spec) — then more
                        // events, and the stream is left open. A client that
                        // buffers the whole body instead of returning on the
                        // matching event hits its request timeout here.
                        let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));
                        sock.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                        )
                        .await
                        .unwrap();
                        write_chunk(
                            &mut sock,
                            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
                        )
                        .await;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        let response = format!(
                            "data: {{\"jsonrpc\":\"2.0\",\"id\":{id},\ndata: \"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"streamed\"}}],\"isError\":false}}}}\n\n"
                        );
                        write_chunk(&mut sock, response.as_bytes()).await;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        write_chunk(
                            &mut sock,
                            b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n",
                        )
                        .await;
                        // Hold the stream open until the client disconnects
                        // (it must have returned early) or 5s as a brake.
                        let mut sink = [0u8; 64];
                        let _ = tokio::time::timeout(Duration::from_secs(5), async {
                            loop {
                                match sock.read(&mut sink).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {}
                                }
                            }
                        })
                        .await;
                    }
                    _ => {}
                }
            }
            seen
        });

        (format!("http://{addr}/mcp"), server)
    }

    /// `connect()` reads the real proxy env; these tests must dial loopback
    /// directly. Serialize with the proxy tests (same lock as fetch.rs) and
    /// scrub the proxy env vars; [`restore_proxy_env`] puts them back.
    async fn scrub_proxy_env() -> (
        tokio::sync::MutexGuard<'static, ()>,
        Vec<(String, Option<String>)>,
    ) {
        let guard = crate::proxy::test_util::LOCK.lock().await;
        let saved: Vec<(String, Option<String>)> = [
            "CONGA_TOOL_PROXY",
            "CONGA_LLM_PROXY",
            "HTTPS_PROXY",
            "https_proxy",
        ]
        .iter()
        .map(|k| (k.to_string(), std::env::var(k).ok()))
        .collect();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }
        (guard, saved)
    }

    fn restore_proxy_env(saved: &[(String, Option<String>)]) {
        for (k, v) in saved {
            if let Some(v) = v {
                std::env::set_var(k, v);
            }
        }
    }

    /// End-to-end against a local raw-HTTP MCP server: the session id from
    /// the initialize response is echoed on every later request, and a
    /// `tools/call` answered by an SSE stream that stays open still returns
    /// on the matching event (no whole-body buffering, no draining).
    #[tokio::test]
    async fn http_session_id_captured_echoed_and_sse_returns_early() {
        let (_guard, saved) = scrub_proxy_env().await;
        let (url, server) = fake_mcp_http_server(Some("sess-abc-123")).await;
        // Short timeout: a client that buffers the SSE body instead of
        // returning on the matching event fails the call instead of hanging.
        let connected =
            McpHttpClient::connect("fake", &url, &HashMap::new(), Duration::from_secs(2)).await;
        restore_proxy_env(&saved);

        let (client, tools) = connected.expect("connect");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "mcp__fake__echo");

        let result = (tools[0].execute)(tool_call_ctx_for_test(
            "c1",
            serde_json::json!({"text": "hello"}),
        ))
        .await
        .expect("tools/call over a still-open SSE stream");
        assert!(!result.is_error);
        assert!(matches!(
            &result.content[0],
            ContentBlock::Text { text } if text == "streamed"
        ));

        let seen = server.await.unwrap();
        assert_eq!(
            seen.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(),
            vec![
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
        // initialize itself carries no session id (nothing captured yet);
        // every later request echoes the server-assigned one.
        assert_eq!(seen[0].session_id, None);
        for r in &seen[1..] {
            assert_eq!(r.session_id.as_deref(), Some("sess-abc-123"));
        }
        drop(client);
    }

    /// A stateless server (no Mcp-Session-Id on initialize) must never
    /// receive an invented one.
    #[tokio::test]
    async fn http_stateless_server_gets_no_session_header() {
        let (_guard, saved) = scrub_proxy_env().await;
        let (url, server) = fake_mcp_http_server(None).await;
        let connected =
            McpHttpClient::connect("fake", &url, &HashMap::new(), Duration::from_secs(2)).await;
        let (_client, tools) = connected.expect("connect");
        let result = (tools[0].execute)(tool_call_ctx_for_test(
            "c1",
            serde_json::json!({"text": "hello"}),
        ))
        .await
        .expect("tools/call");
        assert!(!result.is_error);
        restore_proxy_env(&saved);

        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 4);
        assert!(
            seen.iter().all(|r| r.session_id.is_none()),
            "stateless server must not receive Mcp-Session-Id: {seen:?}"
        );
    }

    // ── Integration test (Python mock server) ────────────────────

    fn fixture_mcp_server_script() -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_echo.py");
        std::fs::write(
            &path,
            r#"#!/usr/bin/env python3
import sys, json

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method", "")

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "echo-server", "version": "1.0.0"}
        }})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [{
            "name": "echo",
            "description": "echo back the arguments",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }
        }]}})
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        text = args.get("text", "")
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "content": [{"type": "text", "text": text}],
            "isError": False
        }})
"#,
        )
        .unwrap();
        let kept = dir.keep();
        kept.join("mcp_echo.py").to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn mcp_handshake_list_and_call() {
        let script = fixture_mcp_server_script();
        let env = HashMap::new();
        let (bridge, tools) =
            McpBridge::spawn("test", "python3", &[script], &env, Duration::from_secs(10))
                .await
                .expect("spawn");

        // tools/list → 1 tool, prefixed
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "mcp__test__echo");
        assert_eq!(tools[0].label, "test/echo");
        assert_eq!(tools[0].risk, RiskLevel::High);

        // tools/call → echo back
        let result = (tools[0].execute)(tool_call_ctx_for_test(
            "c1",
            serde_json::json!({"text": "hello mcp"}),
        ))
        .await
        .unwrap();
        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hello mcp"),
            _ => panic!("expected text content"),
        }
        drop(bridge);
    }

    /// Helper: build a ToolCallCtx for tests.
    fn tool_call_ctx_for_test(id: &str, args: serde_json::Value) -> conga::ToolCallCtx {
        conga::ToolCallCtx {
            tool_call_id: id.into(),
            args,
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
            },
        }
    }

    // ── Smoke test: real GitHub MCP server (needs token + network) ──────

    /// End-to-end against the real `@modelcontextprotocol/server-github`.
    /// Ignored by default — run with:
    ///   GITHUB_PERSONAL_ACCESS_TOKEN=ghp_xxx \
    ///     cargo test -p conga-host -- --ignored mcp_smoke_github
    #[tokio::test]
    #[ignore]
    async fn mcp_smoke_github() {
        let token = std::env::var("GITHUB_PERSONAL_ACCESS_TOKEN")
            .expect("set GITHUB_PERSONAL_ACCESS_TOKEN to run this smoke test");
        let mut env = HashMap::new();
        env.insert("GITHUB_PERSONAL_ACCESS_TOKEN".into(), token);

        let (bridge, tools) = McpBridge::spawn(
            "github",
            "npx",
            &["-y".into(), "@modelcontextprotocol/server-github".into()],
            &env,
            Duration::from_secs(30),
        )
        .await
        .expect("spawn github mcp");

        // GitHub MCP exposes dozens of tools; we just need > 0.
        assert!(!tools.is_empty(), "expected tools from github server");
        eprintln!("(github mcp: {} tools discovered)", tools.len());

        // Verify naming convention on the first tool.
        assert!(
            tools[0].name.starts_with("mcp__github__"),
            "tool name not prefixed: {}",
            tools[0].name
        );

        // Call search_repositories — read-only, no specific repo needed,
        // verifies the token works and the full tools/call path.
        let search = tools
            .iter()
            .find(|t| t.name == "mcp__github__search_repositories")
            .expect("search_repositories tool not found");

        let result = (search.execute)(tool_call_ctx_for_test(
            "smoke",
            serde_json::json!({"query": "conga"}),
        ))
        .await
        .expect("search_repositories call");
        assert!(
            !result.is_error,
            "search_repositories returned error: {:?}",
            result.content
        );
        eprintln!(
            "(search_repositories ok, {} content blocks)",
            result.content.len()
        );
        drop(bridge);
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::*;

    /// Serializes these tests: they touch the process-global tool-proxy
    /// override (conga's own test lock is pub(crate), unavailable here).
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
    }

    #[test]
    fn tool_proxy_wins_over_legacy_env() {
        let _g = LOCK.lock().unwrap();
        crate::proxy::set_tool_proxy(Some("socks5://tool:1080")).unwrap();
        assert_eq!(
            pick_mcp_proxy(&fake_env(&[
                ("CONGA_TOOL_PROXY", "socks5://tool:1080"),
                ("CONGA_LLM_PROXY", "http://llm:8080"),
            ])),
            Some("socks5://tool:1080".to_string())
        );
        crate::proxy::set_tool_proxy(None).unwrap();
    }

    #[test]
    fn legacy_llm_proxy_still_works() {
        let _g = LOCK.lock().unwrap();
        crate::proxy::set_tool_proxy(None).unwrap();
        assert_eq!(
            pick_mcp_proxy(&fake_env(&[("CONGA_LLM_PROXY", "http://llm:8080")])),
            Some("http://llm:8080".to_string())
        );
        // Real env may carry CONGA_TOOL_PROXY on dev machines; the None branch
        // is only deterministic when it's unset.
        if std::env::var("CONGA_TOOL_PROXY").is_err() {
            assert_eq!(super::pick_mcp_proxy(&fake_env(&[])), None);
        }
    }
}
