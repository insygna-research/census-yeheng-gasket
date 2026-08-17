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

/// One long-lived MCP HTTP client. Each tool's `execute` closure holds an
/// `Arc<McpHttpClient>`; calls are stateless POSTs to the server URL.
pub struct McpHttpClient {
    client: reqwest::Client,
    url: String,
    headers: HashMap<String, String>,
    timeout: Duration,
    server_name: String,
    next_id: tokio::sync::Mutex<u64>,
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

        // Parse based on Content-Type.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            // SSE: read body, find the `data:` line matching our id.
            let text = resp
                .text()
                .await
                .map_err(|e| McpError::Protocol(format!("http body read error: {e}")))?;
            self.parse_sse_response(&text, id)
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

    /// Parse an SSE response body, extracting the JSON-RPC message matching `id`.
    fn parse_sse_response(&self, text: &str, id: u64) -> Result<serde_json::Value, McpError> {
        for line in text.lines() {
            let line = line.trim();
            // SSE data lines start with "data:".
            let Some(json_str) = line.strip_prefix("data:") else {
                continue;
            };
            let json_str = json_str.trim();
            if json_str.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(json_str) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match parse_jsonrpc_value(&v, id) {
                Parsed::Result(result) => return Ok(result),
                Parsed::Error { code, message } => {
                    return Err(McpError::ServerError { code, message });
                }
                Parsed::Other => continue,
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
        // Simulate an SSE body: a notification line, then the matching response.
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"isError\":false}}\n\n";
        // We need a dummy McpHttpClient to call parse_sse_response, but that
        // requires a reqwest::Client. Instead, test the logic inline: the
        // function just scans `data:` lines and calls parse_jsonrpc_value.
        // Verify the core logic by extracting data lines manually.
        let matching = sse
            .lines()
            .filter_map(|l| l.trim().strip_prefix("data:"))
            .map(|s| s.trim())
            .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .find_map(|v| match parse_jsonrpc_value(&v, 7) {
                Parsed::Result(r) => Some(r),
                _ => None,
            })
            .expect("should find matching response");
        assert_eq!(matching["content"][0]["text"], "hi");
        assert_eq!(matching["isError"], false);
    }

    #[test]
    fn sse_response_missing_match_returns_error() {
        let sse = "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n";
        let found = sse
            .lines()
            .filter_map(|l| l.trim().strip_prefix("data:"))
            .map(|s| s.trim())
            .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .find_map(|v| match parse_jsonrpc_value(&v, 7) {
                Parsed::Result(r) => Some(r),
                _ => None,
            });
        assert!(found.is_none(), "should not find a match for id=7");
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
