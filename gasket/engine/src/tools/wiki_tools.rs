//! Unified wiki tools for LLM interaction.
//!
//! Provides clean read/write/search interface over the wiki knowledge base.
//! LLM does not need to know about Tantivy or SQLite internals.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, instrument};

use super::{simple_schema, Tool, ToolContext, ToolError, ToolResult};
use crate::wiki::{PageIndex, PageStore, PageType, WikiPage};

// ── WikiSearchTool ───────────────────────────────────────────────

/// Search wiki pages using Tantivy BM25.
pub struct WikiSearchTool {
    page_store: PageStore,
    page_index: Arc<PageIndex>,
}

impl WikiSearchTool {
    pub fn new(page_store: PageStore, page_index: Arc<PageIndex>) -> Self {
        Self {
            page_store,
            page_index,
        }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    10
}

#[async_trait]
impl Tool for WikiSearchTool {
    fn name(&self) -> &str {
        "wiki_search"
    }

    fn description(&self) -> &str {
        "Search the wiki knowledge base using full-text search. Returns matching pages with titles, paths, and relevance scores."
    }

    fn parameters(&self) -> Value {
        simple_schema(&[
            ("query", "string", true, "Search query text"),
            (
                "limit",
                "integer",
                false,
                "Maximum number of results (default: 10)",
            ),
        ])
    }

    #[instrument(name = "tool.wiki_search", skip_all)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let parsed: SearchArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("Invalid arguments: {}", e)))?;

        if parsed.query.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "query must not be empty".to_string(),
            ));
        }

        debug!(
            "wiki_search: query='{}' limit={}",
            parsed.query, parsed.limit
        );

        // JIT watermark reconciliation before search.
        if let Err(e) = self.page_store.sync_db_from_disk().await {
            tracing::warn!("wiki_search: JIT sync failed (continuing): {}", e);
        }

        let hits = self
            .page_index
            .search_with_store(&parsed.query, parsed.limit, Some(&self.page_store))
            .await
            .map_err(|e| ToolError::ExecutionError(format!("Search failed: {}", e)))?;

        if hits.is_empty() {
            return Ok(format!("No wiki pages found matching '{}'.", parsed.query));
        }

        let mut output = format!(
            "Found {} wiki page{} matching '{}':\n\n",
            hits.len(),
            if hits.len() == 1 { "" } else { "s" },
            parsed.query
        );

        for summary in &hits {
            let tags_display = if summary.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", summary.tags.join(", "))
            };
            let summary_display = summary.summary.as_deref().unwrap_or("(no summary)");
            output.push_str(&format!(
                "━━━ {} ━━━\n  Summary: {}\n  Type: {} | Confidence: {:.1}{}\n  Path: {}\n\n",
                summary.title,
                summary_display,
                summary.page_type.as_str(),
                summary.confidence,
                tags_display,
                summary.path
            ));
        }

        Ok(output)
    }
}

// ── WikiWriteTool ────────────────────────────────────────────────

/// Write or update a wiki page.
pub struct WikiWriteTool {
    page_store: PageStore,
}

impl WikiWriteTool {
    pub fn new(page_store: PageStore) -> Self {
        Self { page_store }
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    title: String,
    content: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_page_type")]
    page_type: String,
}

fn default_page_type() -> String {
    "topic".to_string()
}

#[async_trait]
impl Tool for WikiWriteTool {
    fn name(&self) -> &str {
        "wiki_write"
    }

    fn description(&self) -> &str {
        "Write or update a wiki page. Creates a new page if it does not exist, overwrites if it does. \
         The page is persisted to SQLite and indexed in Tantivy immediately."
    }

    fn parameters(&self) -> Value {
        simple_schema(&[
            (
                "path",
                "string",
                true,
                "Wiki page path (e.g. 'topics/rust-async', 'entities/projects/gasket')",
            ),
            ("title", "string", true, "Page title"),
            ("content", "string", true, "Markdown content"),
            (
                "page_type",
                "string",
                false,
                "Page type: topic (default), entity, source, sop",
            ),
            (
                "tags",
                "array",
                false,
                "Optional tags (e.g. [\"rust\", \"async\"])",
            ),
        ])
    }

    #[instrument(name = "tool.wiki_write", skip_all)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let parsed: WriteArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("Invalid arguments: {}", e)))?;

        let path = parsed.path.trim();
        let title = parsed.title.trim();
        let content = parsed.content.trim();

        if path.is_empty() {
            return Err(ToolError::InvalidArguments(
                "path must not be empty".to_string(),
            ));
        }

        const ALLOWED_PREFIXES: &[&str] = &["topics/", "entities/", "sources/", "sops/"];
        if !ALLOWED_PREFIXES.iter().any(|p| path.starts_with(p)) {
            return Err(ToolError::InvalidArguments(format!(
                "Wiki path MUST start with one of: topics/, entities/, sources/, sops/. \
                 Do not write to the wiki root. Got: '{}'",
                path
            )));
        }

        if title.is_empty() {
            return Err(ToolError::InvalidArguments(
                "title must not be empty".to_string(),
            ));
        }

        let pt = match parsed.page_type.to_lowercase().as_str() {
            "entity" => PageType::Entity,
            "source" => PageType::Source,
            "sop" => PageType::Sop,
            _ => PageType::Topic,
        };
        let pt_str = pt.as_str();

        let mut page = WikiPage::new(path.to_string(), title.to_string(), pt, content.to_string());
        page.tags = parsed.tags;

        self.page_store.write(&page).await.map_err(|e| {
            ToolError::ExecutionError(format!("Failed to write wiki page '{}': {}", path, e))
        })?;

        Ok(format!(
            "Wiki page written: {} [{}] at {}",
            title, pt_str, path
        ))
    }
}

// ── WikiDeleteTool ───────────────────────────────────────────────

/// Delete a wiki page.
pub struct WikiDeleteTool {
    page_store: PageStore,
}

impl WikiDeleteTool {
    pub fn new(page_store: PageStore) -> Self {
        Self { page_store }
    }
}

#[derive(Deserialize)]
struct DeleteArgs {
    path: String,
}

#[async_trait]
impl Tool for WikiDeleteTool {
    fn name(&self) -> &str {
        "wiki_delete"
    }

    fn description(&self) -> &str {
        "Delete a wiki page by path. Removes both the disk file and the SQLite record."
    }

    fn parameters(&self) -> Value {
        simple_schema(&[(
            "path",
            "string",
            true,
            "Wiki page path to delete (e.g. 'topics/rust-async')",
        )])
    }

    #[instrument(name = "tool.wiki_delete", skip_all)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let parsed: DeleteArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("Invalid arguments: {}", e)))?;

        let path = parsed.path.trim();
        if path.is_empty() {
            return Err(ToolError::InvalidArguments(
                "path must not be empty".to_string(),
            ));
        }

        self.page_store.delete(path).await.map_err(|e| {
            ToolError::ExecutionError(format!("Failed to delete wiki page '{}': {}", path, e))
        })?;

        Ok(format!("Wiki page deleted: {}", path))
    }
}

// ── WikiReadTool ─────────────────────────────────────────────────

/// Read a wiki page from SQLite.
pub struct WikiReadTool {
    page_store: PageStore,
}

impl WikiReadTool {
    pub fn new(page_store: PageStore) -> Self {
        Self { page_store }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
}

#[async_trait]
impl Tool for WikiReadTool {
    fn name(&self) -> &str {
        "wiki_read"
    }

    fn description(&self) -> &str {
        "Read a wiki page by path. Returns the full Markdown content and metadata."
    }

    fn parameters(&self) -> Value {
        simple_schema(&[(
            "path",
            "string",
            true,
            "Wiki page path (e.g. 'topics/rust-async')",
        )])
    }

    #[instrument(name = "tool.wiki_read", skip_all)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let parsed: ReadArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("Invalid arguments: {}", e)))?;

        let path = parsed.path.trim();
        if path.is_empty() {
            return Err(ToolError::InvalidArguments(
                "path must not be empty".to_string(),
            ));
        }

        // JIT: if this specific file is newer than the watermark, trigger
        // incremental sync so SQLite and search index are consistent.
        let disk_path = self.page_store.wiki_root().join(format!("{}.md", path));
        if let Ok(mtime) = PageStore::file_mtime(&disk_path).await {
            if let Ok(Some(wm_str)) = self.page_store.kv_read_watermark().await {
                if let Ok(watermark) = wm_str.parse::<i64>() {
                    if mtime > watermark {
                        if let Err(e) = self.page_store.sync_db_from_disk().await {
                            tracing::warn!("wiki_read: JIT sync failed (continuing): {}", e);
                        }
                    }
                }
            }
        }

        let page = self.page_store.read(path).await.map_err(|e| {
            ToolError::ExecutionError(format!("Failed to read wiki page '{}': {}", path, e))
        })?;

        let tags_display = if page.tags.is_empty() {
            String::new()
        } else {
            format!("\nTags: [{}]", page.tags.join(", "))
        };

        Ok(format!(
            "━━━ {} ━━━\nPath: {}\nType: {} | Updated: {}{}{}\n\n{}",
            page.title,
            page.path,
            page.page_type.as_str(),
            page.updated.format("%Y-%m-%d %H:%M UTC"),
            tags_display,
            page.category
                .map(|c| format!("\nCategory: {}", c))
                .unwrap_or_default(),
            page.content
        ))
    }
}
