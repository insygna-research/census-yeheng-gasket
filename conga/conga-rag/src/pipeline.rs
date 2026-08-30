//! Ingest pipeline: scan → clean → chunk → embed → store.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use sha2::{Digest, Sha256};

use crate::chunk::{chunk, Chunk};
use crate::clean::clean;
use crate::config::RagConfig;
use crate::embed::EmbeddingsClient;
use crate::source::DirSource;
use crate::store::{DocRow, Store};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct IngestStats {
    pub scanned: usize,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub chunks: usize,
}

/// A changed document waiting to be embedded and upserted.
struct Pending {
    source: String,
    path: PathBuf,
    mtime: i64,
    hash: String,
    chunks: Vec<Chunk>,
    /// Present in the store before this run (→ updated, not added).
    existed: bool,
}

/// Ingest every configured source (or just `only`): scan → remove deleted →
/// skip unchanged (mtime, then content hash) → clean → chunk → embed (one
/// flat batch for all pending docs of the run) → upsert.
pub async fn run_ingest(
    cfg: &RagConfig,
    only: Option<&str>,
    rebuild: bool,
) -> anyhow::Result<IngestStats> {
    let mut stats = IngestStats::default();
    let db_path = cfg.store_path();
    if rebuild && db_path.exists() {
        fs::remove_file(&db_path).context("删除旧库失败(--rebuild)")?;
    }
    let resolved = cfg.resolve_embedding()?;
    let client = EmbeddingsClient::new(&resolved);
    let store = Store::open(&db_path)?;
    let mut pending: Vec<Pending> = Vec::new();

    for (name, src_cfg) in &cfg.sources {
        if let Some(o) = only {
            if o != name {
                continue;
            }
        }
        let dir = DirSource::new(name, src_cfg)?;
        let files = dir.scan()?;
        stats.scanned += files.len();
        let existing: HashMap<PathBuf, DocRow> = store
            .docs_for_source(name)?
            .into_iter()
            .map(|d| (d.path.clone(), d))
            .collect();

        // Removals first.
        stats.removed += store.remove_missing(
            name,
            &files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        )?;

        // Collect pending (changed) docs.
        for f in &files {
            // Nanosecond granularity: second-precision mtimes would falsely
            // skip a rewrite that lands within the same second.
            let mtime = fs::metadata(&f.path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            if let Some(prev) = existing.get(&f.path) {
                if prev.mtime == mtime {
                    stats.skipped += 1;
                    continue;
                }
            }
            let raw = match fs::read_to_string(&f.path) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("跳过不可读文件 {}: {e}", f.rel);
                    stats.failed += 1;
                    continue;
                }
            };
            let cleaned = clean(&raw);
            let hash = hex(&Sha256::digest(cleaned.as_bytes()));
            if let Some(prev) = existing.get(&f.path) {
                if prev.content_hash == hash {
                    store.touch_mtime(name, &f.path, mtime)?;
                    stats.skipped += 1;
                    continue;
                }
            }
            let chunks = chunk(
                &cleaned,
                cfg.chunking.target_chars,
                cfg.chunking.overlap_chars,
            );
            let existed = existing.contains_key(&f.path);
            pending.push(Pending {
                source: name.clone(),
                path: f.path.clone(),
                mtime,
                hash,
                chunks,
                existed,
            });
        }
    }

    // Embed all pending chunks (across sources) in shared flat batches.
    if !pending.is_empty() {
        let flat: Vec<String> = pending
            .iter()
            .flat_map(|p| p.chunks.iter().map(|c| c.content.clone()))
            .collect();
        let vectors = client
            .embed_batch(&flat)
            .await
            .context("embedding 调用失败")?;
        if !flat.is_empty() {
            let first_dim = vectors.first().map(|v| v.len()).unwrap_or(0);
            store.ensure_vec(first_dim, &resolved.model)?;
        }
        let mut cursor = 0usize;
        for p in &pending {
            let n = p.chunks.len();
            let parts: Vec<(usize, String, Vec<f32>)> = p
                .chunks
                .iter()
                .enumerate()
                .zip(&vectors[cursor..cursor + n])
                .map(|((ordinal, c), v)| (ordinal, c.content.clone(), v.clone()))
                .collect();
            store.upsert_doc(&p.source, &p.path, p.mtime, &p.hash, &parts)?;
            stats.chunks += n;
            if p.existed {
                stats.updated += 1;
            } else {
                stats.added += 1;
            }
            cursor += n;
        }
    }
    Ok(stats)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::config::{RagConfig, SourceConfig};

    async fn test_cfg(dir: &std::path::Path, db: &std::path::Path, base_url: &str) -> RagConfig {
        let mut sources = BTreeMap::new();
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
            embedding: crate::config::EmbeddingConfig {
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
    async fn ingest_is_idempotent_and_removes_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        let (base, _req) = crate::testsupport::spawn_mock_embeddings(0).await;
        let cfg = test_cfg(dir.path(), &dbdir.path().join("t.db"), &base).await;

        std::fs::write(dir.path().join("match.md"), "# T\n\nmatch one").unwrap();
        std::fs::write(dir.path().join("other.md"), "other text").unwrap();

        let s1 = run_ingest(&cfg, None, false).await.unwrap();
        assert_eq!((s1.scanned, s1.added, s1.failed), (2, 2, 0));
        assert!(s1.chunks >= 2);

        let s2 = run_ingest(&cfg, None, false).await.unwrap();
        assert_eq!((s2.added, s2.updated, s2.removed, s2.chunks), (0, 0, 0, 0));
        assert_eq!(s2.skipped, 2);

        std::fs::remove_file(dir.path().join("other.md")).unwrap();
        std::fs::write(dir.path().join("match.md"), "# T\n\nmatch two").unwrap();
        let s3 = run_ingest(&cfg, None, false).await.unwrap();
        assert_eq!((s3.removed, s3.added, s3.updated), (1, 0, 1));
    }

    #[tokio::test]
    async fn rebuild_wipes_and_recreates() {
        let dir = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        let db = dbdir.path().join("t.db");
        let (base, _req) = crate::testsupport::spawn_mock_embeddings(0).await;
        let cfg = test_cfg(dir.path(), &db, &base).await;
        std::fs::write(dir.path().join("a.md"), "match").unwrap();
        run_ingest(&cfg, None, false).await.unwrap();
        let s = run_ingest(&cfg, None, true).await.unwrap();
        assert_eq!((s.added, s.scanned), (1, 1));
    }
}
