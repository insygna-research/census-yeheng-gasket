//! SQLite store: documents + chunks + sqlite-vec vec0 KNN, single file.

use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{params, Connection};

/// Search hit, `score` = 1 − cosine distance (higher is better).
#[derive(Debug, Clone)]
pub struct Hit {
    pub source: String,
    pub path: String,
    pub ordinal: usize,
    pub content: String,
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct DocRow {
    pub path: PathBuf,
    pub mtime: i64,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub struct SourceStat {
    pub source: String,
    pub docs: i64,
    pub chunks: i64,
}

pub struct Store {
    conn: Connection,
}

/// Entry-point prototype `sqlite3_auto_extension` expects.
type AutoExtensionEntry = unsafe extern "C" fn(
    *mut rusqlite::ffi::sqlite3,
    *mut *mut std::os::raw::c_char,
    *const rusqlite::ffi::sqlite3_api_routines,
) -> std::os::raw::c_int;

/// Register the sqlite-vec extension exactly once per process, BEFORE any
/// Connection is opened (sqlite3_auto_extension applies to future opens).
fn register_vec_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        // Safety: documented sqlite-vec registration pattern — the extension
        // init function has the auto-extension entry-point ABI; transmuting
        // the fn pointer makes every later-opened connection load vec0.
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            AutoExtensionEntry,
        >(
            sqlite_vec::sqlite3_vec_init as *const ()
        )));
    });
}

/// f32 slice → little-endian byte blob (sqlite-vec binary format).
fn f32_le_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn row_to_hit(r: &rusqlite::Row<'_>) -> rusqlite::Result<Hit> {
    Ok(Hit {
        score: 1.0 - r.get::<_, f64>(1)?,
        source: r.get(2)?,
        path: r.get(3)?,
        ordinal: r.get::<_, i64>(4)? as usize,
        content: r.get(5)?,
    })
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        register_vec_once();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建库目录失败 {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("打开库失败 {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta(
                 key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS documents(
                 id INTEGER PRIMARY KEY,
                 source TEXT NOT NULL,
                 path TEXT NOT NULL,
                 mtime INTEGER NOT NULL,
                 content_hash TEXT NOT NULL,
                 chunk_count INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 UNIQUE(source, path));
             CREATE TABLE IF NOT EXISTS chunks(
                 rowid INTEGER PRIMARY KEY,
                 doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
                 ordinal INTEGER NOT NULL,
                 content TEXT NOT NULL);",
        )?;
        Ok(Store { conn })
    }

    fn meta_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    fn meta_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn fingerprint(&self) -> Option<(String, usize)> {
        let model = self.meta_get("embedding_model").ok()??;
        let dim: usize = self.meta_get("embedding_dim").ok()??.parse().ok()?;
        Some((model, dim))
    }

    pub fn ensure_vec(&self, dim: usize, model: &str) -> anyhow::Result<()> {
        if let Some((m, d)) = self.fingerprint() {
            anyhow::ensure!(
                m == model && d == dim,
                "embedding 指纹变更:库中 {m}[{d}],请求 {model}[{dim}]。请使用 --rebuild 重建索引"
            );
            return Ok(());
        }
        self.conn.execute(
            &format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunks USING vec0(embedding float[{dim}] distance_metric=cosine)"
            ),
            [],
        )?;
        self.meta_set("embedding_model", model)?;
        self.meta_set("embedding_dim", &dim.to_string())?;
        Ok(())
    }

    fn has_vec_table(&self) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='vec_chunks'",
                [],
                |_| Ok(()),
            )
            .is_ok()
    }

    pub fn chunk_count(&self) -> anyhow::Result<i64> {
        if !self.has_vec_table() {
            return Ok(0);
        }
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM vec_chunks", [], |r| r.get(0))?)
    }

    pub fn docs_for_source(&self, source: &str) -> anyhow::Result<Vec<DocRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime, content_hash FROM documents WHERE source = ?1")?;
        let rows = stmt
            .query_map(params![source], |r| {
                Ok(DocRow {
                    path: PathBuf::from(r.get::<_, String>(0)?),
                    mtime: r.get(1)?,
                    content_hash: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn touch_mtime(&self, source: &str, path: &Path, mtime: i64) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE documents SET mtime = ?1 WHERE source = ?2 AND path = ?3",
            params![mtime, source, path.to_string_lossy()],
        )?;
        Ok(())
    }

    /// Document-level upsert in ONE transaction: delete old rows (chunks via
    /// FK cascade, vectors explicitly), then insert fresh.
    pub fn upsert_doc(
        &self,
        source: &str,
        path: &Path,
        mtime: i64,
        hash: &str,
        chunks: &[(usize, String, Vec<f32>)],
    ) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let p = path.to_string_lossy();
        tx.execute(
            "DELETE FROM vec_chunks WHERE rowid IN (
                 SELECT c.rowid FROM chunks c JOIN documents d ON d.id = c.doc_id
                 WHERE d.source = ?1 AND d.path = ?2)",
            params![source, p],
        )?;
        tx.execute(
            "DELETE FROM documents WHERE source = ?1 AND path = ?2",
            params![source, p],
        )?;
        tx.execute(
            "INSERT INTO documents(source, path, mtime, content_hash, chunk_count, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                source,
                p,
                mtime,
                hash,
                chunks.len() as i64,
                chrono::Utc::now().timestamp()
            ],
        )?;
        let doc_id = tx.last_insert_rowid();
        {
            let mut ins_chunk =
                tx.prepare("INSERT INTO chunks(doc_id, ordinal, content) VALUES(?1, ?2, ?3)")?;
            let mut ins_vec =
                tx.prepare("INSERT INTO vec_chunks(rowid, embedding) VALUES(?1, ?2)")?;
            for (ordinal, content, embedding) in chunks {
                ins_chunk.execute(params![doc_id, *ordinal as i64, content])?;
                let rowid = tx.last_insert_rowid();
                ins_vec.execute(params![rowid, f32_le_blob(embedding)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete documents of `source` whose path is not in `live`. Returns count.
    pub fn remove_missing(&self, source: &str, live: &[PathBuf]) -> anyhow::Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare("SELECT id, path FROM documents WHERE source = ?1")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map(params![source], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        let mut removed = 0;
        for (id, path) in rows {
            if !live.iter().any(|p| p.to_string_lossy() == path) {
                tx.execute(
                    "DELETE FROM vec_chunks WHERE rowid IN (
                         SELECT c.rowid FROM chunks c WHERE c.doc_id = ?1)",
                    params![id],
                )?;
                tx.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
                removed += 1;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// KNN over vec0. Errors when the index is empty (no vec table).
    pub fn knn(&self, query: &[f32], k: usize, source: Option<&str>) -> anyhow::Result<Vec<Hit>> {
        if !self.has_vec_table() {
            anyhow::bail!("索引为空:请先运行 conga-rag ingest");
        }
        // Source filter is bound as ?3 (never string-interpolated). vec0
        // pre-filters KNN candidates via `rowid IN (SELECT ...)`.
        let (filter, src): (&str, Option<&str>) = match source {
            Some(s) => (
                " AND rowid IN (SELECT c.rowid FROM chunks c \
                 JOIN documents d ON d.id = c.doc_id WHERE d.source = ?3)",
                Some(s),
            ),
            None => ("", None),
        };
        let sql = format!(
            "SELECT k.rowid, k.distance, d.source, d.path, c.ordinal, c.content
             FROM (SELECT rowid, distance FROM vec_chunks
                   WHERE embedding MATCH ?1 AND k = ?2{filter}) k
             JOIN chunks c ON c.rowid = k.rowid
             JOIN documents d ON d.id = c.doc_id
             ORDER BY k.distance"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let hits = match src {
            Some(s) => stmt
                .query_map(params![f32_le_blob(query), k as i64, s], row_to_hit)?
                .collect::<Result<Vec<_>, _>>()?,
            None => stmt
                .query_map(params![f32_le_blob(query), k as i64], row_to_hit)?
                .collect::<Result<Vec<_>, _>>()?,
        };
        Ok(hits)
    }

    pub fn stats(&self) -> anyhow::Result<Vec<SourceStat>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.source, count(DISTINCT d.id), count(c.rowid)
             FROM documents d LEFT JOIN chunks c ON c.doc_id = d.id
             GROUP BY d.source ORDER BY d.source",
        )?;
        let out = stmt
            .query_map([], |r| {
                Ok(SourceStat {
                    source: r.get(0)?,
                    docs: r.get(1)?,
                    chunks: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("t.db")).unwrap();
        (dir, s)
    }

    fn dim4(v: [f32; 4]) -> Vec<f32> {
        v.to_vec()
    }

    #[test]
    fn open_creates_schema() {
        let (_d, s) = store();
        assert_eq!(s.chunk_count().unwrap(), 0);
        assert!(s.fingerprint().is_none());
    }

    #[test]
    fn ensure_vec_is_idempotent_and_locks_fingerprint() {
        let (_d, s) = store();
        s.ensure_vec(4, "m1").unwrap();
        s.ensure_vec(4, "m1").unwrap(); // idempotent
        assert_eq!(s.fingerprint().unwrap(), ("m1".to_string(), 4));
        let err = s.ensure_vec(8, "m2").unwrap_err();
        assert!(err.to_string().contains("--rebuild"), "err: {err}");
    }

    #[test]
    fn upsert_and_knn_roundtrip() {
        let (_d, s) = store();
        s.ensure_vec(4, "m1").unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/a.md"),
            1,
            "h1",
            &[
                (0, "alpha content".into(), dim4([1.0, 0.0, 0.0, 0.0])),
                (1, "beta content".into(), dim4([0.0, 1.0, 0.0, 0.0])),
            ],
        )
        .unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/b.md"),
            1,
            "h2",
            &[(0, "gamma content".into(), dim4([0.0, 0.0, 1.0, 0.0]))],
        )
        .unwrap();

        let hits = s.knn(&dim4([1.0, 0.0, 0.0, 0.0]), 2, None).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "/n/a.md");
        assert_eq!(hits[0].ordinal, 0);
        assert_eq!(hits[0].content, "alpha content");
        assert!(
            hits[0].score > 0.99,
            "identical vector → score≈1: {}",
            hits[0].score
        );
        assert!(hits[0].score >= hits[1].score);

        // source filter narrows results
        let _ = s
            .upsert_doc(
                "other",
                Path::new("/n/c.md"),
                1,
                "h3",
                &[(0, "delta content".into(), dim4([1.0, 0.0, 0.0, 0.0]))],
            )
            .unwrap();
        let only = s
            .knn(&dim4([1.0, 0.0, 0.0, 0.0]), 5, Some("other"))
            .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].source, "other");
    }

    #[test]
    fn upsert_replaces_previous_chunks() {
        let (_d, s) = store();
        s.ensure_vec(4, "m1").unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/a.md"),
            1,
            "h1",
            &[(0, "old".into(), dim4([1.0, 0.0, 0.0, 0.0]))],
        )
        .unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/a.md"),
            2,
            "h1b",
            &[
                (0, "new".into(), dim4([0.0, 0.9, 0.0, 0.0])),
                (1, "new2".into(), dim4([0.0, 0.9, 0.1, 0.0])),
            ],
        )
        .unwrap();
        let hits = s.knn(&dim4([1.0, 0.0, 0.0, 0.0]), 10, None).unwrap();
        assert_eq!(hits.len(), 2, "old vector must be gone");
        assert!(hits.iter().all(|h| h.content.starts_with("new")));
        assert_eq!(s.chunk_count().unwrap(), 2);
    }

    #[test]
    fn remove_missing_deletes_only_absent() {
        let (_d, s) = store();
        s.ensure_vec(4, "m1").unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/a.md"),
            1,
            "h1",
            &[(0, "a".into(), dim4([1.0, 0.0, 0.0, 0.0]))],
        )
        .unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/gone.md"),
            1,
            "h2",
            &[(0, "g".into(), dim4([0.0, 1.0, 0.0, 0.0]))],
        )
        .unwrap();
        let removed = s
            .remove_missing("src", &[PathBuf::from("/n/a.md")])
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(s.chunk_count().unwrap(), 1);
        let rows = s.docs_for_source("src").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, Path::new("/n/a.md"));
    }

    #[test]
    fn knn_without_vec_table_is_error() {
        let (_d, s) = store();
        let err = s.knn(&dim4([0.0; 4]), 3, None).unwrap_err();
        assert!(err.to_string().contains("ingest"), "err: {err}");
    }

    #[test]
    fn stats_groups_by_source() {
        let (_d, s) = store();
        s.ensure_vec(4, "m1").unwrap();
        s.upsert_doc(
            "a",
            Path::new("/x/1"),
            1,
            "h",
            &[(0, "c".into(), dim4([1.0, 0.0, 0.0, 0.0]))],
        )
        .unwrap();
        s.upsert_doc(
            "b",
            Path::new("/x/2"),
            1,
            "h",
            &[
                (0, "c".into(), dim4([0.0, 1.0, 0.0, 0.0])),
                (1, "c2".into(), dim4([0.0, 0.0, 1.0, 0.0])),
            ],
        )
        .unwrap();
        let st = s.stats().unwrap();
        assert_eq!(st.len(), 2);
        let b = st.iter().find(|x| x.source == "b").unwrap();
        assert_eq!((b.docs, b.chunks), (1, 2));
    }

    #[test]
    fn touch_mtime_updates_row() {
        let (_d, s) = store();
        s.ensure_vec(4, "m1").unwrap();
        s.upsert_doc(
            "s",
            Path::new("/n/a"),
            10,
            "h",
            &[(0, "c".into(), dim4([1.0, 0.0, 0.0, 0.0]))],
        )
        .unwrap();
        s.touch_mtime("s", Path::new("/n/a"), 99).unwrap();
        let rows = s.docs_for_source("s").unwrap();
        assert_eq!(rows[0].mtime, 99);
        assert_eq!(rows[0].content_hash, "h");
    }
}
