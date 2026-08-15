//! Web search tool with multiple providers.
//!
//! Environment variables:
//! - GASKET_SEARCH_PROVIDER: "duckduckgo" (default, no key required), "serper",
//!   "serpapi", "brave", "tavily", "exa", or "firecrawl"
//! - GASKET_SERPER_API_KEY: Serper.dev API key
//! - GASKET_SERPAPI_API_KEY: SerpAPI API key
//! - GASKET_BRAVE_API_KEY: Brave Search API key
//! - GASKET_TAVILY_API_KEY: Tavily API key
//! - GASKET_EXA_API_KEY: Exa API key
//! - GASKET_FIRECRAWL_API_KEY: Firecrawl API key
//! - GASKET_TOOL_PROXY: optional proxy (http/https/socks5/socks5h) for search traffic

use std::sync::Arc;

use gasket_core::{ContentBlock, ExtensionApi, RiskLevel, ToolDefinition, ToolError, ToolResult};
use reqwest::Client;
use serde::Deserialize;
use tracing::{info, warn};

// ── Search result abstraction ────────────────────────────────────────────

struct SearchHit {
    title: String,
    snippet: String,
    url: String,
}

const MAX_SNIPPET_LEN: usize = 300;

fn format_hits(hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return "No results found.".to_string();
    }
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        let snippet = if h.snippet.chars().count() > MAX_SNIPPET_LEN {
            format!(
                "{}...",
                h.snippet.chars().take(MAX_SNIPPET_LEN).collect::<String>()
            )
        } else {
            h.snippet.clone()
        };
        out.push_str(&format!(
            "{}. **{}**\n   {}\n   URL: {}\n\n",
            i + 1,
            h.title,
            snippet,
            h.url
        ));
    }
    out
}

// ── Serper.dev response types ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SerperSearchResponse {
    organic: Vec<SerperOrganicResult>,
}

#[derive(Debug, Deserialize)]
struct SerperOrganicResult {
    title: String,
    snippet: String,
    link: String,
}

// ── SerpAPI response types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SerpApiSearchResponse {
    organic_results: Vec<SerpApiOrganicResult>,
}

#[derive(Debug, Deserialize)]
struct SerpApiOrganicResult {
    title: String,
    snippet: Option<String>,
    link: String,
}

// ── DuckDuckGo response parsing ──────────────────────────────────────────

async fn search_duckduckgo(
    client: &Client,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    );

    let response = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .send()
        .await
        .map_err(|e| ToolError::Message(format!("DuckDuckGo request failed: {}", e)))?;

    check_status(&response, "DuckDuckGo").await?;

    let html = response
        .text()
        .await
        .map_err(|e| ToolError::Message(format!("DuckDuckGo response read failed: {}", e)))?;

    parse_duckduckgo_html(&html, count)
}

fn parse_duckduckgo_html(html: &str, count: usize) -> Result<Vec<SearchHit>, ToolError> {
    let doc = dom_query::Document::from(html);
    let mut hits = Vec::new();

    for node in doc.select(".result").iter() {
        if hits.len() >= count {
            break;
        }

        let title_sel = node.select(".result__a").iter().next();
        let raw_href = title_sel
            .as_ref()
            .and_then(|n| n.attr("href"))
            .unwrap_or_default();
        let url = extract_duckduckgo_url(&raw_href);
        let title = title_sel
            .map(|n| n.text().trim().to_string())
            .unwrap_or_default();
        let snippet = node
            .select(".result__snippet")
            .iter()
            .next()
            .map(|n| n.text().trim().to_string())
            .unwrap_or_default();

        if !url.is_empty() && !title.is_empty() {
            hits.push(SearchHit {
                title,
                snippet,
                url,
            });
        }
    }

    Ok(hits)
}

/// Extract the real target URL from a DuckDuckGo redirect link.
/// DuckDuckGo wraps result links like `//duckduckgo.com/l/?uddg=...`.
fn extract_duckduckgo_url(href: &str) -> String {
    if href.is_empty() {
        return String::new();
    }

    if let Some(query_start) = href.find('?') {
        let query = &href[query_start + 1..];
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                if key == "uddg" {
                    if let Ok(decoded) = urlencoding::decode(value) {
                        return decoded.into_owned();
                    }
                }
            }
        }
    }

    if href.starts_with("//") {
        return format!("https:{}", href);
    }

    href.to_string()
}

// ── Brave response types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BraveSearchResponse {
    web: BraveWebResults,
}

#[derive(Debug, Deserialize)]
struct BraveWebResults {
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    title: String,
    description: String,
    url: String,
}

async fn search_brave(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencoding::encode(query),
        count
    );

    let resp: BraveSearchResponse = send_get(
        client,
        &url,
        Some(("X-Subscription-Token", api_key)),
        "Brave",
    )
    .await?;

    Ok(resp
        .web
        .results
        .into_iter()
        .map(|r| SearchHit {
            title: r.title,
            snippet: r.description,
            url: r.url,
        })
        .collect())
}

// ── Tavily response types ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TavilySearchResponse {
    results: Vec<TavilyResult>,
}

#[derive(Debug, Deserialize)]
struct TavilyResult {
    title: String,
    content: String,
    url: String,
}

async fn search_tavily(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let body = serde_json::json!({
        "api_key": api_key,
        "query": query,
        "max_results": count,
        "search_depth": "basic"
    });

    let resp: TavilySearchResponse = send_post_json(
        client,
        "https://api.tavily.com/search",
        &body,
        None,
        "Tavily",
    )
    .await?;

    Ok(resp
        .results
        .into_iter()
        .map(|r| SearchHit {
            title: r.title,
            snippet: r.content,
            url: r.url,
        })
        .collect())
}

// ── Exa response types ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ExaSearchResponse {
    results: Vec<ExaResult>,
}

#[derive(Debug, Deserialize)]
struct ExaResult {
    title: Option<String>,
    text: Option<String>,
    url: String,
}

async fn search_exa(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let body = serde_json::json!({
        "query": query,
        "numResults": count,
        "contents": { "text": true }
    });

    let resp: ExaSearchResponse = send_post_json(
        client,
        "https://api.exa.ai/search",
        &body,
        Some(("x-api-key", api_key)),
        "Exa",
    )
    .await?;

    Ok(resp
        .results
        .into_iter()
        .map(|r| SearchHit {
            title: r.title.unwrap_or_else(|| "No title".to_string()),
            snippet: r
                .text
                .map(|t| t.chars().take(MAX_SNIPPET_LEN).collect())
                .unwrap_or_else(|| "No description".to_string()),
            url: r.url,
        })
        .collect())
}

// ── Firecrawl response types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FirecrawlSearchResponse {
    data: Vec<FirecrawlResult>,
}

#[derive(Debug, Deserialize)]
struct FirecrawlResult {
    title: Option<String>,
    description: Option<String>,
    url: String,
}

async fn search_firecrawl(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let body = serde_json::json!({
        "query": query,
        "limit": count
    });

    let resp: FirecrawlSearchResponse = send_post_json(
        client,
        "https://api.firecrawl.dev/v1/search",
        &body,
        Some(("Authorization", &format!("Bearer {}", api_key))),
        "Firecrawl",
    )
    .await?;

    Ok(resp
        .data
        .into_iter()
        .map(|r| SearchHit {
            title: r.title.unwrap_or_else(|| "No title".to_string()),
            snippet: r
                .description
                .unwrap_or_else(|| "No description".to_string()),
            url: r.url,
        })
        .collect())
}

// ── Shared HTTP helpers ──────────────────────────────────────────────────

async fn send_get<T: serde::de::DeserializeOwned>(
    client: &Client,
    url: &str,
    auth_header: Option<(&str, &str)>,
    provider_name: &str,
) -> Result<T, ToolError> {
    let mut req = client.get(url).header("Accept", "application/json");

    if let Some((key, value)) = auth_header {
        req = req.header(key, value);
    }

    let response = req.send().await.map_err(|e| {
        ToolError::Message(format!(
            "{} API request failed: {}",
            provider_name,
            e.without_url()
        ))
    })?;

    check_status(&response, provider_name).await?;

    response.json::<T>().await.map_err(|e| {
        ToolError::Message(format!(
            "Failed to parse {} API response: {}",
            provider_name, e
        ))
    })
}

async fn send_post_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    url: &str,
    body: &serde_json::Value,
    auth_header: Option<(&str, &str)>,
    provider_name: &str,
) -> Result<T, ToolError> {
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .json(body);

    if let Some((key, value)) = auth_header {
        req = req.header(key, value);
    }

    let response = req.send().await.map_err(|e| {
        ToolError::Message(format!(
            "{} API request failed: {}",
            provider_name,
            e.without_url()
        ))
    })?;

    check_status(&response, provider_name).await?;

    response.json::<T>().await.map_err(|e| {
        ToolError::Message(format!(
            "Failed to parse {} API response: {}",
            provider_name, e
        ))
    })
}

async fn check_status(response: &reqwest::Response, provider_name: &str) -> Result<(), ToolError> {
    if !response.status().is_success() {
        return Err(ToolError::Message(format!(
            "{} API error (status {})",
            provider_name,
            response.status()
        )));
    }
    Ok(())
}

// ── Existing provider implementations ────────────────────────────────────

async fn search_serper(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let body = serde_json::json!({
        "q": query,
        "num": count,
    });

    let resp: SerperSearchResponse = send_post_json(
        client,
        "https://google.serper.dev/search",
        &body,
        Some(("X-API-KEY", api_key)),
        "Serper",
    )
    .await?;

    Ok(resp
        .organic
        .into_iter()
        .map(|r| SearchHit {
            title: r.title,
            snippet: r.snippet,
            url: r.link,
        })
        .collect())
}

async fn search_serpapi(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let url = format!(
        "https://serpapi.com/search?api_key={}&engine=google&q={}&num={}",
        api_key,
        urlencoding::encode(query),
        count
    );

    let resp: SerpApiSearchResponse = send_get(client, &url, None, "SerpAPI").await?;

    Ok(resp
        .organic_results
        .into_iter()
        .map(|r| SearchHit {
            title: r.title,
            snippet: r.snippet.unwrap_or_else(|| "No description".to_string()),
            url: r.link,
        })
        .collect())
}

/// Reads `env_var`; on success runs `f`, otherwise reports a "not set" error.
async fn with_api_key<F, Fut>(env_var: &str, f: F) -> Result<Vec<SearchHit>, ToolError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<SearchHit>, ToolError>>,
{
    match std::env::var(env_var) {
        Ok(api_key) => f(api_key).await,
        Err(_) => Err(ToolError::Message(format!("{} not set", env_var))),
    }
}

// ── Tool implementation ──────────────────────────────────────────────────

pub fn register(api: &mut dyn ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "web_search".into(),
        label: "Web Search".into(),
        description: "Search the web for current information. Supported providers: duckduckgo (default), serper, serpapi, brave, tavily, exa, firecrawl. Configure via GASKET_SEARCH_PROVIDER and the corresponding GASKET_<PROVIDER>_API_KEY environment variable.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "count": { "type": "number", "description": "Number of results to return (default 5)", "default": 5 }
            },
            "required": ["query"]
        }),
        risk: RiskLevel::High,
        execute: Arc::new(move |ctx| {
            Box::pin(async move {
                // Built per call so the runtime tool proxy (desktop UI /
                // GASKET_TOOL_PROXY) applies without re-registering.
                let client = gasket_core::apply_tool_proxy(Client::builder())
                    .build()
                    .map_err(|e| ToolError::Message(format!("client build failed: {e}")))?;
                let query = ctx.args["query"].as_str().unwrap_or_default();
                let count = ctx.args["count"].as_u64().unwrap_or(5) as usize;

                info!("Web search: query='{}', count={}", query, count);

                let provider_name = std::env::var("GASKET_SEARCH_PROVIDER").unwrap_or_else(|_| "duckduckgo".to_string());

                let hits_result = match provider_name.to_lowercase().as_str() {
                    "serper" => {
                        with_api_key("GASKET_SERPER_API_KEY", |key| async move {
                            search_serper(&client, &key, query, count).await
                        })
                        .await
                    }
                    "serpapi" => {
                        with_api_key("GASKET_SERPAPI_API_KEY", |key| async move {
                            search_serpapi(&client, &key, query, count).await
                        })
                        .await
                    }
                    "duckduckgo" => search_duckduckgo(&client, query, count).await,
                    "brave" => {
                        with_api_key("GASKET_BRAVE_API_KEY", |key| async move {
                            search_brave(&client, &key, query, count).await
                        })
                        .await
                    }
                    "tavily" => {
                        with_api_key("GASKET_TAVILY_API_KEY", |key| async move {
                            search_tavily(&client, &key, query, count).await
                        })
                        .await
                    }
                    "exa" => {
                        with_api_key("GASKET_EXA_API_KEY", |key| async move {
                            search_exa(&client, &key, query, count).await
                        })
                        .await
                    }
                    "firecrawl" => {
                        with_api_key("GASKET_FIRECRAWL_API_KEY", |key| async move {
                            search_firecrawl(&client, &key, query, count).await
                        })
                        .await
                    }
                    _ => Err(ToolError::Message(format!(
                        "Unknown search provider: {}. Use 'serper', 'serpapi', 'duckduckgo', 'brave', 'tavily', 'exa', or 'firecrawl'.", provider_name
                    ))),
                };

                match hits_result {
                    Ok(hits) => Ok(ToolResult {
                        content: vec![ContentBlock::text(format_hits(&hits))],
                        details: serde_json::json!({ "result_count": hits.len() }),
                        is_error: false,
                    }),
                    Err(e) => {
                        warn!("Search failed: {}", e);
                        Ok(ToolResult {
                            content: vec![ContentBlock::text(format!("Search failed: {}", e))],
                            details: serde_json::json!({}),
                            is_error: true,
                        })
                    }
                }
            })
        }),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Vendor JSON shape contracts ──────────────────────────────
    // Each test pins the serde field mapping against a canned response in the
    // vendor's real shape. A vendor renaming a field fails here, not in prod.

    #[test]
    fn serper_response_maps_fields() {
        let resp: SerperSearchResponse = serde_json::from_str(
            r#"{"organic":[{"title":"Rust Lang","snippet":"A language","link":"https://rust-lang.org"}]}"#,
        )
        .unwrap();
        assert_eq!(resp.organic.len(), 1);
        assert_eq!(resp.organic[0].title, "Rust Lang");
        assert_eq!(resp.organic[0].snippet, "A language");
        assert_eq!(resp.organic[0].link, "https://rust-lang.org");
    }

    #[test]
    fn serpapi_response_optional_snippet() {
        let resp: SerpApiSearchResponse = serde_json::from_str(
            r#"{"organic_results":[{"title":"T","link":"https://t.example"}]}"#,
        )
        .unwrap();
        assert_eq!(resp.organic_results.len(), 1);
        assert_eq!(resp.organic_results[0].snippet, None);
        assert_eq!(resp.organic_results[0].title, "T");
        assert_eq!(resp.organic_results[0].link, "https://t.example");
    }

    #[test]
    fn brave_response_maps_fields() {
        let resp: BraveSearchResponse = serde_json::from_str(
            r#"{"web":{"results":[{"title":"B","description":"D","url":"https://b.example"}]}}"#,
        )
        .unwrap();
        assert_eq!(resp.web.results.len(), 1);
        assert_eq!(resp.web.results[0].title, "B");
        assert_eq!(resp.web.results[0].description, "D");
        assert_eq!(resp.web.results[0].url, "https://b.example");
    }

    #[test]
    fn tavily_response_maps_fields() {
        let resp: TavilySearchResponse = serde_json::from_str(
            r#"{"results":[{"title":"V","content":"C","url":"https://v.example"}]}"#,
        )
        .unwrap();
        assert_eq!(resp.results.len(), 1);
        assert_eq!(resp.results[0].title, "V");
        assert_eq!(resp.results[0].content, "C");
        assert_eq!(resp.results[0].url, "https://v.example");
    }

    #[test]
    fn exa_response_optional_fields() {
        let resp: ExaSearchResponse =
            serde_json::from_str(r#"{"results":[{"url":"https://e.example"}]}"#).unwrap();
        assert_eq!(resp.results.len(), 1);
        assert_eq!(resp.results[0].title, None);
        assert_eq!(resp.results[0].text, None);
        assert_eq!(resp.results[0].url, "https://e.example");
    }

    #[test]
    fn firecrawl_response_optional_fields() {
        let resp: FirecrawlSearchResponse =
            serde_json::from_str(r#"{"data":[{"url":"https://f.example"}]}"#).unwrap();
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].title, None);
        assert_eq!(resp.data[0].description, None);
        assert_eq!(resp.data[0].url, "https://f.example");
    }

    // ── DuckDuckGo HTML parsing ──────────────────────────────────

    fn ddg_fixture() -> &'static str {
        r#"<html><body>
          <div class="result">
            <h2 class="result__title">
              <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc">Example Page</a>
            </h2>
            <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage">The example snippet.</a>
          </div>
          <div class="result">
            <h2 class="result__title">
              <a class="result__a" href="https://plain.example.org/direct">Plain Link</a>
            </h2>
            <a class="result__snippet" href="https://plain.example.org/direct">Second snippet.</a>
          </div>
          <div class="result">
            <h2 class="result__title"><span>No Link Here</span></h2>
          </div>
        </body></html>"#
    }

    #[test]
    fn parse_duckduckgo_html_extracts_results() {
        let hits = parse_duckduckgo_html(ddg_fixture(), 10).unwrap();
        assert_eq!(hits.len(), 2, "the link-less result must be skipped");
        assert_eq!(hits[0].title, "Example Page");
        assert_eq!(hits[0].url, "https://example.com/page");
        assert_eq!(hits[0].snippet, "The example snippet.");
        assert_eq!(hits[1].title, "Plain Link");
        assert_eq!(hits[1].url, "https://plain.example.org/direct");
    }

    #[test]
    fn parse_duckduckgo_html_respects_count() {
        let hits = parse_duckduckgo_html(ddg_fixture(), 1).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Example Page");
    }

    #[test]
    fn extract_duckduckgo_url_decodes_uddg() {
        assert_eq!(
            extract_duckduckgo_url(
                "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=x"
            ),
            "https://example.com/a?b=1"
        );
        // Plain https link passes through untouched.
        assert_eq!(
            extract_duckduckgo_url("https://plain.example.org/direct"),
            "https://plain.example.org/direct"
        );
        // Protocol-relative link gets https:.
        assert_eq!(
            extract_duckduckgo_url("//protocol.example.org"),
            "https://protocol.example.org"
        );
        assert_eq!(extract_duckduckgo_url(""), "");
    }

    // ── Output formatting ────────────────────────────────────────

    #[test]
    fn format_hits_empty() {
        assert_eq!(format_hits(&[]), "No results found.");
    }

    #[test]
    fn format_hits_truncates_long_snippets() {
        let long = "x".repeat(400);
        let out = format_hits(&[SearchHit {
            title: "T".into(),
            snippet: long.clone(),
            url: "https://u".into(),
        }]);
        assert!(
            out.contains("..."),
            "long snippet must be truncated with ellipsis"
        );
        assert!(!out.contains(&long), "full snippet must not appear");
        assert!(out.contains("1. **T**"));
        assert!(out.contains("https://u"));
    }

    /// Wiring proof: the tool must build its HTTP client at execute time so
    /// the runtime tool proxy applies. A minimal in-test HTTP proxy observes
    /// the tool's traffic. DuckDuckGo is fetched over https, so a conforming
    /// client opens a CONNECT tunnel through the proxy (absolute-form GET
    /// only happens for plain-http targets); the fake proxy grants the
    /// tunnel but speaks no TLS, so the search surfaces as a tool error —
    /// proving the request went through the proxy, never direct. The
    /// response-parsing path is covered by the `parse_duckduckgo_html`
    /// tests below.
    #[tokio::test]
    async fn web_search_goes_through_tool_proxy() {
        use gasket_core::{ExtensionApiImpl, ToolCallCtx, ToolContext};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
            // Grant the CONNECT tunnel, then close: no TLS is spoken, so a
            // client that (incorrectly) connected directly would never reach
            // this socket at all.
            sock.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            head
        });

        // Mutates process env deliberately: the provider is selected at
        // execute time. No other test in this crate reads env vars.
        std::env::set_var("GASKET_SEARCH_PROVIDER", "duckduckgo");
        gasket_core::set_tool_proxy(Some(&format!("http://{proxy_addr}"))).unwrap();

        let mut api = ExtensionApiImpl::new();
        super::register(&mut api);
        assert_eq!(api.tools.len(), 1);
        assert_eq!(api.tools[0].name, "web_search");
        let tool = api.tools.remove(0);
        let ctx = ToolCallCtx {
            tool_call_id: "t1".into(),
            args: serde_json::json!({"query": "rust language", "count": 2}),
            signal: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
                spawner: None,
            },
        };
        let result = (tool.execute)(ctx).await.unwrap();

        // Cleanup before assertions so a failed assert can't leak state.
        gasket_core::set_tool_proxy(None).unwrap();
        std::env::remove_var("GASKET_SEARCH_PROVIDER");

        assert!(result.is_error, "the TLS-less tunnel must fail: {result:?}");
        match &result.content[0] {
            gasket_core::ContentBlock::Text { text } => {
                assert!(text.contains("Search failed"), "got: {text}");
                assert!(text.contains("https://html.duckduckgo.com/"), "got: {text}");
            }
            _ => panic!("expected text content"),
        }
        // Bounded: with a reverted (registration-time, proxy-less) client the
        // request goes direct and this proxy socket is never reached. Without
        // the timeout the await below would block forever — the assertions
        // above can all pass on a direct-connection failure, so this join is
        // the real wiring check and must fail fast with a diagnosis.
        let head = match tokio::time::timeout(std::time::Duration::from_secs(30), server).await {
            Ok(joined) => joined.unwrap(),
            Err(_) => panic!(
                "proxy socket never reached within 30s: the request did not go \
                 through the tool proxy (is the client still built per call?). The \
                 content assertions above passed on the direct-connection error: \
                 is_error={}, content={:?}",
                result.is_error, result.content
            ),
        };
        assert!(
            head.starts_with("CONNECT html.duckduckgo.com:443 HTTP/1.1"),
            "proxy saw: {head}"
        );
    }
}
