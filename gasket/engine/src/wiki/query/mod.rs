//! Wiki query pipeline — hybrid retrieval with RRF fusion.
//!
//! Phase 1: BM25 + Vector search → candidate sets
//! Phase 2: RRF fusion → merged ranking
//! Phase 3: Budget-aware selection → load full pages from SQLite

use std::sync::Arc;

use anyhow::Result;

use gasket_storage::wiki::{
    slugify, PageSearchIndex, PageStore, PageSummary, PageType, SearchHit, WikiPage,
};

use super::indexing_service::{WikiEmbeddingProvider, WikiVectorHit, WikiVectorStore};

/// RRF constant (standard value from Cormack et al., 2009).
const RRF_K: u32 = 60;

/// Token budget for query results (controls how much content to return).
#[derive(Debug, Clone)]
pub struct TokenBudget {
    pub max_tokens: usize,
}

impl TokenBudget {
    pub fn new(max_tokens: usize) -> Self {
        Self { max_tokens }
    }

    pub fn default_budget() -> Self {
        Self { max_tokens: 4000 }
    }

    pub fn chars_budget(&self) -> usize {
        self.max_tokens * 4
    }
}

impl Default for TokenBudget {
    fn default() -> Self {
        Self::default_budget()
    }
}

/// Result of a wiki query.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub pages: Vec<WikiPage>,
    pub total_candidates: usize,
    pub estimated_tokens: usize,
}

impl QueryResult {
    pub fn to_context_string(&self) -> String {
        let mut out = String::new();
        for page in &self.pages {
            if !out.is_empty() {
                out.push_str("\n---\n");
            }
            out.push_str(&format!(
                "## {} ({})\n\n{}",
                page.title, page.path, page.content
            ));
        }
        out
    }
}

/// Wiki query engine — hybrid retrieval over wiki pages.
///
/// When semantic search is configured, uses RRF (Reciprocal Rank Fusion)
/// to merge BM25 and vector search results. Falls back to pure BM25
/// when no embedding provider is available.
pub struct WikiQueryEngine {
    search: Arc<dyn PageSearchIndex>,
    store: PageStore,
    embedding_provider: Option<Arc<dyn WikiEmbeddingProvider>>,
    vector_store: Option<Arc<dyn WikiVectorStore>>,
}

impl WikiQueryEngine {
    pub fn new(search: Arc<dyn PageSearchIndex>, store: PageStore) -> Self {
        Self {
            search,
            store,
            embedding_provider: None,
            vector_store: None,
        }
    }

    /// Attach semantic search capabilities for hybrid retrieval.
    pub fn with_semantic(
        mut self,
        provider: Arc<dyn WikiEmbeddingProvider>,
        store: Arc<dyn WikiVectorStore>,
    ) -> Self {
        self.embedding_provider = Some(provider);
        self.vector_store = Some(store);
        self
    }

    /// Full hybrid query with RRF fusion and budget-aware selection.
    pub async fn query(&self, query: &str, budget: TokenBudget) -> Result<QueryResult> {
        // JIT watermark reconciliation: ensure any out-of-band disk edits
        // are incrementally compiled into SQLite before we search.
        if let Err(e) = self.store.sync_db_from_disk().await {
            tracing::warn!("WikiQueryEngine: JIT sync failed (continuing): {}", e);
        }
        let candidates = self.hybrid_search(query, 50).await?;
        let total_candidates = candidates.len();

        if candidates.is_empty() {
            return Ok(QueryResult {
                pages: vec![],
                total_candidates: 0,
                estimated_tokens: 0,
            });
        }

        let paths: Vec<String> = candidates.iter().map(|h| h.path.clone()).collect();
        let summaries = self.store.read_summaries(&paths).await?;

        let summary_by_path: std::collections::HashMap<&str, &PageSummary> =
            summaries.iter().map(|s| (s.path.as_str(), s)).collect();

        let chars_budget = budget.chars_budget();
        let mut used_chars = 0usize;
        let mut selected_paths = Vec::new();
        let mut estimated_tokens = 0usize;

        for hit in &candidates {
            let Some(summary) = summary_by_path.get(hit.path.as_str()) else {
                tracing::debug!("WikiQuery: summary not found for '{}'", hit.path);
                continue;
            };
            let page_chars = summary.content_length as usize;
            if used_chars + page_chars > chars_budget && !selected_paths.is_empty() {
                break;
            }
            used_chars += page_chars;
            estimated_tokens += page_chars / 4;
            selected_paths.push(hit.path.as_str());
        }

        let selected_paths_owned: Vec<String> =
            selected_paths.into_iter().map(|s| s.to_string()).collect();
        let pages = self.store.read_many(&selected_paths_owned).await?;

        Ok(QueryResult {
            pages,
            total_candidates,
            estimated_tokens,
        })
    }

    /// Hybrid search: merge BM25 and vector results via RRF.
    /// Falls back to pure BM25 when semantic search is not configured.
    async fn hybrid_search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        match (&self.embedding_provider, &self.vector_store) {
            (Some(provider), Some(vstore)) => {
                // Parallel BM25 + vector search.
                let bm25_future = self.search.search(query, limit);
                let query_vec = provider.embed(query).await?;
                let vector_future = vstore.search(&query_vec, limit, 0.3);

                let (bm25_results, vector_results) = tokio::join!(bm25_future, vector_future);

                let bm25_hits = bm25_results.unwrap_or_default();
                let vector_hits = vector_results.unwrap_or_default();

                Ok(rrf_merge(&bm25_hits, &vector_hits))
            }
            _ => {
                // Pure BM25 fallback.
                self.search.search(query, limit).await
            }
        }
    }

    /// Simple BM25 search returning search hits.
    pub async fn search_raw(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        self.search.search(query, limit).await
    }

    /// File a good answer back into the wiki as a new topic page.
    pub async fn file_answer(&self, question: &str, answer: &str) -> Result<String> {
        let path = format!("topics/{}", slugify(question));
        let page = WikiPage::new(
            path.clone(),
            question.to_string(),
            PageType::Topic,
            answer.to_string(),
        );
        self.store.write(&page).await?;

        let full_page = self.store.read(&path).await?;
        self.search
            .upsert(&gasket_storage::wiki::IndexPage {
                path: full_page.path.clone(),
                title: full_page.title.clone(),
                content: full_page.content.clone(),
                page_type: full_page.page_type.as_str().to_string(),
                category: full_page.category.clone(),
                tags: full_page.tags.clone(),
                summary: full_page.summary.clone(),
                confidence: full_page.confidence,
            })
            .await?;

        tracing::info!("Filed answer as wiki page: '{}'", path);
        Ok(path)
    }

    /// Rebuild the Tantivy index from all pages in the store.
    pub async fn rebuild_index(&self) -> Result<usize> {
        let summaries = self.store.list(Default::default()).await?;
        let mut index_pages = Vec::new();
        for summary in &summaries {
            if let Ok(page) = self.store.read(&summary.path).await {
                index_pages.push(gasket_storage::wiki::IndexPage {
                    path: page.path.clone(),
                    title: page.title.clone(),
                    content: page.content.clone(),
                    page_type: page.page_type.as_str().to_string(),
                    category: page.category.clone(),
                    tags: page.tags.clone(),
                    summary: page.summary.clone(),
                    confidence: page.confidence,
                });
            }
        }
        let count = index_pages.len();
        self.search.rebuild(&index_pages).await?;
        Ok(count)
    }

    pub fn search(&self) -> &Arc<dyn PageSearchIndex> {
        &self.search
    }

    pub fn store(&self) -> &PageStore {
        &self.store
    }
}

/// Merge BM25 and vector search results using Reciprocal Rank Fusion.
///
/// Formula: score = 1/(k + rank_bm25) + 1/(k + rank_vector)
/// Where k=60 (standard from Cormack, Clarke & Buettcher, 2009).
fn rrf_merge(bm25_hits: &[SearchHit], vector_hits: &[WikiVectorHit]) -> Vec<SearchHit> {
    let mut scores: std::collections::HashMap<String, f32> = std::collections::HashMap::new();

    for (rank, hit) in bm25_hits.iter().enumerate() {
        let rank = rank as u32 + 1; // 1-based rank
        *scores.entry(hit.path.clone()).or_default() += 1.0 / (RRF_K + rank) as f32;
    }

    for (rank, hit) in vector_hits.iter().enumerate() {
        let rank = rank as u32 + 1;
        *scores.entry(hit.id.clone()).or_default() += 1.0 / (RRF_K + rank) as f32;
    }

    let mut merged: Vec<(String, f32)> = scores.into_iter().collect();
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    merged
        .into_iter()
        .map(|(path, score)| SearchHit {
            path,
            score,
            title: String::new(), // Title will be filled when loading full pages.
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_budget_default() {
        let budget = TokenBudget::default();
        assert_eq!(budget.max_tokens, 4000);
        assert_eq!(budget.chars_budget(), 16000);
    }

    #[test]
    fn test_query_result_to_context_string() {
        let result = QueryResult {
            pages: vec![WikiPage::new(
                "topics/rust".to_string(),
                "Rust".to_string(),
                PageType::Topic,
                "Rust is a systems language.".to_string(),
            )],
            total_candidates: 5,
            estimated_tokens: 6,
        };
        let ctx = result.to_context_string();
        assert!(ctx.contains("## Rust"));
        assert!(ctx.contains("topics/rust"));
        assert!(ctx.contains("systems language"));
    }

    #[test]
    fn test_token_budget_custom() {
        let budget = TokenBudget::new(1000);
        assert_eq!(budget.max_tokens, 1000);
        assert_eq!(budget.chars_budget(), 4000);
    }
}
