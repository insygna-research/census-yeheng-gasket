use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;

/// Deterministic embeddings mock: every text gets a 4-dim vector derived from
/// its length; `[1,0,0,0]` for texts containing the word "match" so retrieval
/// tests are predictable. Records request count; fails the first N requests
/// with 429 when `fail_first` > 0.
pub async fn spawn_mock(fail_first: u32) -> (String, Arc<AtomicU32>) {
    let state = Arc::new(AtomicU32::new(fail_first));
    let requests = Arc::new(AtomicU32::new(0));
    let req_clone = requests.clone();
    let fail_clone = state.clone();

    let app = Router::new().route(
        "/v1/embeddings",
        post(move |Json(body): Json<Value>| {
            let req_clone = req_clone.clone();
            let fail_clone = fail_clone.clone();
            async move {
                req_clone.fetch_add(1, Ordering::SeqCst);
                if fail_clone.load(Ordering::SeqCst) > 0 {
                    fail_clone.fetch_sub(1, Ordering::SeqCst);
                    return (StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response();
                }
                let texts = body["input"].as_array().unwrap();
                let data: Vec<Value> = texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let s = t.as_str().unwrap_or("");
                        let v = if s.contains("match") {
                            vec![1.0f64, 0.0, 0.0, 0.0]
                        } else {
                            vec![0.0f64, (s.len() as f64) / 1000.0, 0.0, 0.0]
                        };
                        serde_json::json!({"index": i, "embedding": v})
                    })
                    .collect();
                (StatusCode::OK, Json(serde_json::json!({"data": data}))).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), requests)
}
