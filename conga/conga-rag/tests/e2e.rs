//! End-to-end smoke: fixtures on disk → mock embeddings → ingest ×2 → search.

use conga_rag::config::{EmbeddingConfig, RagConfig, SourceConfig};
use conga_rag::pipeline::run_ingest;
use conga_rag::search::run_search;

fn write_cfg(dir: &std::path::Path, db: &std::path::Path, base_url: &str) -> RagConfig {
    let mut sources = std::collections::BTreeMap::new();
    sources.insert(
        "notes".to_string(),
        SourceConfig {
            kind: "dir".into(),
            path: dir.to_path_buf(),
            include: vec!["**/*.md".into()],
            exclude: vec![],
        },
    );
    RagConfig {
        sources,
        embedding: EmbeddingConfig {
            base_url: Some(base_url.into()),
            api_key: Some("k".into()),
            model: Some("mock".into()),
            batch: 4,
        },
        ..Default::default()
    }
    .with_store_path(db.to_path_buf())
}

#[tokio::test]
async fn end_to_end_ingest_search() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hit.md"), "# Doc\n\nmatch the answer here").unwrap();
    std::fs::write(dir.path().join("miss.md"), "完全无关的内容").unwrap();
    let (base, _req) = conga_rag::testsupport::spawn_mock_embeddings(0).await;
    let dbdir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), &dbdir.path().join("e2e.db"), &base);

    let s1 = run_ingest(&cfg, None, false).await.unwrap();
    assert_eq!((s1.added, s1.failed), (2, 0));
    let s2 = run_ingest(&cfg, None, false).await.unwrap();
    assert_eq!((s2.added, s2.updated, s2.removed), (0, 0, 0), "idempotent");

    let hits = run_search(&cfg, "match", 2, None).await.unwrap();
    assert!(!hits.is_empty());
    assert!(
        hits[0].content.contains("match"),
        "top hit must be the match doc"
    );
}
