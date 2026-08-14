//! `fetch` tool — HTTP GET a URL, convert HTML to readable markdown text.

use std::sync::Arc;
use std::time::Duration;

use crate::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};
use crate::ContentBlock;

const TIMEOUT_SECS: u64 = 30;
const MAX_OUTPUT_BYTES: usize = 200_000;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "fetch".into(),
        label: "Fetch".into(),
        description: "Fetch a URL and return its content as readable text. Converts HTML to markdown-like text (headings, links, lists). http/https only.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http/https URL to fetch" }
            },
            "required": ["url"]
        }),
        risk: RiskLevel::Low,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, crate::error::ToolError> {
    if ctx.aborted() {
        return Ok(ToolResult::error("aborted".to_string()));
    }
    let url = ctx.args["url"]
        .as_str()
        .ok_or_else(|| crate::error::ToolError::Message("url is required".into()))?;

    // Reject non-http(s) schemes early — defends against file:///etc/passwd etc.
    let scheme = url.split("://").next().unwrap_or("").to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Ok(ToolResult::error(format!(
            "unsupported URL scheme '{scheme}': only http/https allowed"
        )));
    }

    let client = crate::proxy::apply_tool_proxy(reqwest::Client::builder())
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .user_agent("gasket-fetch/1.0")
        .build()
        .map_err(|e| crate::error::ToolError::Message(format!("client build failed: {e}")))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| crate::error::ToolError::Message(format!("request failed: {e}")))?;

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let status = resp.status();
    if !status.is_success() {
        return Ok(ToolResult::error(format!("HTTP {status}")));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| crate::error::ToolError::Message(format!("read body failed: {e}")))?;

    let text = if content_type.contains("html") {
        html_to_text(&body)
    } else {
        // Non-HTML: return raw text (JSON, plain text, etc.)
        body
    };

    let text = truncate(&text);
    Ok(ToolResult {
        content: vec![ContentBlock::text(text)],
        details: serde_json::json!({"url": url, "content_type": content_type}),
        is_error: false,
    })
}

/// Convert HTML to readable plain text: strip non-content elements, extract
/// text from `<article>`/`<main>`/`<body>` in that priority.
fn html_to_text(html: &str) -> String {
    let doc = dom_query::Document::from(html);

    // Remove non-content noise elements.
    doc.select("script, style, nav, footer, header, noscript, iframe, svg")
        .remove();

    // Prefer semantic content containers; fall back to body.
    let content_sel = ["article", "main", "body"].into_iter().find_map(|sel| {
        let nodes = doc.select(sel);
        if nodes.iter().count() > 0 {
            Some(nodes)
        } else {
            None
        }
    });

    let raw = match content_sel {
        Some(nodes) => nodes
            .iter()
            .next()
            .map(|n| n.text().trim().to_string())
            .unwrap_or_default(),
        None => doc.select("body").text().trim().to_string(),
    };

    // Collapse excessive whitespace (HTML text often has lots of blank lines).
    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Truncate to MAX_OUTPUT_BYTES (char-safe), appending an indicator.
fn truncate(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    let cut = s[..MAX_OUTPUT_BYTES]
        .char_indices()
        .last()
        .map(|(i, _)| i)
        .unwrap_or(MAX_OUTPUT_BYTES);
    let mut out = s[..cut].to_string();
    out.push_str("\n...(truncated)");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_script_and_style() {
        let html = r#"<html><body>
            <script>alert('xss')</script>
            <style>body { color: red; }</style>
            <p>Hello world</p>
        </body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Hello world"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color"));
    }

    #[test]
    fn html_to_text_prefers_article() {
        let html = r#"<html><body>
            <nav>Menu</nav>
            <article><p>Main content here</p></article>
        </body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Main content here"));
        assert!(!text.contains("Menu"));
    }

    #[test]
    fn html_to_text_falls_back_to_body() {
        let html = r#"<html><body><p>No article or main tag</p></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("No article or main tag"));
    }

    #[test]
    fn truncate_adds_indicator() {
        let big = "x".repeat(MAX_OUTPUT_BYTES + 1000);
        let out = truncate(&big);
        assert!(out.ends_with("...(truncated)"));
        assert!(out.len() < big.len());
    }

    #[test]
    fn truncate_noop_under_limit() {
        let s = "small text";
        assert_eq!(truncate(s), s);
    }

    #[tokio::test]
    async fn fetch_rejects_non_http_scheme() {
        let ctx = ToolCallCtx {
            tool_call_id: "t1".into(),
            args: serde_json::json!({"url": "file:///etc/passwd"}),
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: crate::ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
                spawner: None,
            },
        };
        let result = execute(ctx).await.unwrap();
        assert!(result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("unsupported URL scheme"));
            }
            _ => panic!("expected text content"),
        }
    }

    /// End-to-end wiring proof: with the override set, fetch's request must
    /// hit the proxy, not the target host. A real HTTP proxy in ~25 lines:
    /// read the request head, reply with a canned page.
    #[tokio::test]
    async fn fetch_goes_through_tool_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _g = crate::proxy::test_util::LOCK.lock().unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut head = String::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                head.push_str(&String::from_utf8_lossy(&buf[..n]));
                if head.contains("\r\n\r\n") {
                    break;
                }
            }
            let body = "<html><body><article>via proxy</article></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            head
        });

        crate::proxy::set_tool_proxy(Some(&format!("http://{proxy_addr}"))).unwrap();
        let ctx = ToolCallCtx {
            tool_call_id: "t2".into(),
            args: serde_json::json!({"url": "http://example.test/"}),
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: crate::ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
                spawner: None,
            },
        };
        let result = execute(ctx).await.unwrap();
        crate::proxy::set_tool_proxy(None).unwrap();

        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("via proxy")),
            _ => panic!("expected text content"),
        }
        // A proxied http request carries the absolute target URI on the
        // request line — proof the connection went through the proxy.
        let head = server.await.unwrap();
        assert!(head.starts_with("GET http://example.test/"), "proxy saw: {head}");
    }
}
