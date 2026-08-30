use conga_rag::testsupport::spawn_mock_embeddings;

use conga::RetryPolicy;
use conga_rag::config::ResolvedEmbedding;
use conga_rag::embed::EmbeddingsClient;

fn cfg() -> ResolvedEmbedding {
    ResolvedEmbedding {
        base_url: "unused".into(),
        api_key: "k".into(),
        model: "mock".into(),
        batch: 2,
    }
}

fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        max_retries: 3,
        initial_delay_ms: 10,
        max_delay_ms: 50,
        jitter: false,
    }
}

#[tokio::test]
async fn batches_are_split_by_config() {
    let (base, requests) = spawn_mock_embeddings(0).await;
    let client = EmbeddingsClient::with_retry(&cfg(), &base, fast_retry());
    let texts: Vec<String> = (0..5).map(|i| format!("doc text {i}")).collect();
    let out = client.embed_batch(&texts).await.unwrap();
    assert_eq!(out.len(), 5);
    assert!(out.iter().all(|v| v.len() == 4));
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "5 texts / batch 2 → 3 requests"
    );
}

#[tokio::test]
async fn retry_on_429_then_success() {
    let (base, requests) = spawn_mock_embeddings(1).await;
    let client = EmbeddingsClient::with_retry(&cfg(), &base, fast_retry());
    let out = client.embed_batch(&["one".to_string()]).await.unwrap();
    assert_eq!(out.len(), 1);
    assert!(
        requests.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "first 429 must be retried"
    );
}

#[tokio::test]
async fn non_retryable_error_propagates_fast() {
    // fail_first=999 keeps failing; retries exhaust and error surfaces
    let (base, requests) = spawn_mock_embeddings(999).await;
    let client = EmbeddingsClient::with_retry(&cfg(), &base, fast_retry());
    let err = client.embed_batch(&["one".to_string()]).await.unwrap_err();
    assert!(err.is_retryable() || err.to_string().contains("429"));
    assert!(requests.load(std::sync::atomic::Ordering::SeqCst) >= 2);
}
