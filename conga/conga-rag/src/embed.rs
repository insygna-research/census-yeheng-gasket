//! OpenAI-compatible /embeddings client: batching + retry + validation.

use conga::RetryPolicy;
use serde::Deserialize;

use crate::config::ResolvedEmbedding;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("embeddings API HTTP {status}")]
    Http { status: u16, retryable: bool },
    #[error("embedding 响应条数不符:期望 {want},实得 {got}")]
    Count { want: usize, got: usize },
    #[error("embedding 维度不一致:首批 {first},第 {idx} 条为 {got}")]
    Dim {
        first: usize,
        idx: usize,
        got: usize,
    },
    #[error("网络错误: {0}")]
    Network(#[source] reqwest::Error),
}

impl EmbedError {
    pub fn is_retryable(&self) -> bool {
        match self {
            EmbedError::Http { retryable, .. } => *retryable,
            EmbedError::Network(_) => true,
            _ => false,
        }
    }
}

#[derive(Deserialize)]
struct EmbResponse {
    data: Vec<EmbItem>,
}

#[derive(Deserialize)]
struct EmbItem {
    index: usize,
    embedding: Vec<f32>,
}

pub struct EmbeddingsClient {
    base_url: String,
    api_key: String,
    model: String,
    batch: usize,
    client: reqwest::Client,
    retry: RetryPolicy,
}

fn default_retry() -> RetryPolicy {
    RetryPolicy {
        max_retries: 2,
        initial_delay_ms: 500,
        max_delay_ms: 8000,
        jitter: true,
    }
}

impl EmbeddingsClient {
    pub fn new(cfg: &ResolvedEmbedding) -> Self {
        Self::with_retry(cfg, &cfg.base_url, default_retry())
    }

    pub fn with_base_url(cfg: &ResolvedEmbedding, base_url: &str) -> Self {
        Self::with_retry(cfg, base_url, default_retry())
    }

    pub fn with_retry(cfg: &ResolvedEmbedding, base_url: &str, retry: RetryPolicy) -> Self {
        Self {
            base_url: base_url.to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            batch: cfg.batch.max(1),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("reqwest client construction with 60s timeout"),
            retry,
        }
    }

    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(self
            .embed_batch(&[text.to_string()])
            .await?
            .into_iter()
            .next()
            .unwrap())
    }

    /// Embed texts in batches of `self.batch`, sequentially.
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.batch) {
            out.extend(self.request_with_retry(chunk).await?);
        }
        Ok(out)
    }

    async fn request_with_retry(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut attempt = 0usize;
        loop {
            match self.one_request(texts).await {
                Ok(v) => return Ok(v),
                Err(e) if attempt < self.retry.max_retries && e.is_retryable() => {
                    let mut delay = self
                        .retry
                        .initial_delay_ms
                        .saturating_mul(1u64 << attempt)
                        .min(self.retry.max_delay_ms);
                    if self.retry.jitter {
                        delay += rand_jitter(delay);
                    }
                    tracing::warn!(attempt, delay_ms = delay, "embeddings 重试: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn one_request(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({"model": self.model, "input": texts}))
            .send()
            .await
            .map_err(EmbedError::Network)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(EmbedError::Http {
                status: status.as_u16(),
                retryable: status.as_u16() == 429 || status.is_server_error(),
            });
        }
        let body: EmbResponse = resp.json().await.map_err(EmbedError::Network)?;
        let mut items = body.data;
        items.sort_by_key(|i| i.index);
        if items.len() != texts.len() {
            return Err(EmbedError::Count {
                want: texts.len(),
                got: items.len(),
            });
        }
        let dim = items.first().map(|i| i.embedding.len()).unwrap_or(0);
        for (idx, item) in items.iter().enumerate() {
            if item.embedding.len() != dim {
                return Err(EmbedError::Dim {
                    first: dim,
                    idx,
                    got: item.embedding.len(),
                });
            }
        }
        Ok(items.into_iter().map(|i| i.embedding).collect())
    }
}

/// Simple jitter without a rand dep: scale by a cheap per-call factor.
fn rand_jitter(base: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    base / 4 + nanos % (base / 4 + 1)
}
