use anyhow::Result;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;
use tokio::fs;

use crate::fs::atomic_write;
use crate::kv_store::KvStore;
use crate::wiki::types::{PageFilter, PageSummary, PageType, WikiPage};

use super::lifecycle::{DecayReport, FrequencyManager};
use super::page_store::WikiPageStore;
use super::types::Frequency;

const WATERMARK_KEY: &str = "wiki_sync_watermark";
const INDEX_DIRTY_KEY: &str = "wiki_index_dirty";
/// Paths whose mtime must never feed the global watermark.
const EXCLUDED_FROM_WATERMARK: &[&str] = &["index", "log"];

/// PageStore: CRUD operations on wiki pages with a **two-layer SSOT contract**.
///
/// - **Disk markdown files are the SSOT for content** (title, type, category,
///   tags, summary, body). Whatever is in `wiki_root/<path>.md` is the truth.
/// - **SQLite is the SSOT for runtime/index state** (access_count, frequency,
///   last_accessed, created/updated timestamps, source_count, confidence).
///   It is also a derived query projection — `list`/`read_many`/`read_summaries`
///   serve their result directly out of SQLite without touching disk.
///
/// `read(path)` enforces the contract: it pulls the markdown off disk first,
/// then overlays the runtime fields from the DB row. This guarantees that
/// out-of-band edits (e.g. `vim wiki_root/topics/foo.md`) are visible to
/// callers, while preserving the runtime stats that only the DB tracks.
///
/// `write(page)` writes disk first, then upserts the DB index — the order
/// matters: if the process crashes between disk-write and DB-upsert, the next
/// `sync_db_from_disk()` reconstructs the missing DB row from disk.
#[derive(Clone)]
pub struct PageStore {
    db: WikiPageStore,
    wiki_root: PathBuf,
    wiki_changed_tx: Option<tokio::sync::mpsc::Sender<String>>,
    kv: KvStore,
}

impl PageStore {
    pub fn new(pool: sqlx::SqlitePool, wiki_root: PathBuf) -> Self {
        Self {
            db: WikiPageStore::new(pool.clone()),
            wiki_root,
            wiki_changed_tx: None,
            kv: KvStore::new(pool),
        }
    }

    /// Attach a channel for publishing wiki-changed notifications.
    /// When set, `write` and `delete` will send the affected path
    /// over this channel instead of touching the global broker.
    pub fn with_wiki_changed_tx(mut self, tx: tokio::sync::mpsc::Sender<String>) -> Self {
        self.wiki_changed_tx = Some(tx);
        self
    }

    /// Get the wiki root directory.
    pub fn wiki_root(&self) -> &PathBuf {
        &self.wiki_root
    }

    /// Run frequency decay batch on all stale pages.
    pub async fn run_decay_batch(&self) -> Result<DecayReport> {
        FrequencyManager::run_decay_batch(&self.db).await
    }

    /// Get metadata for a page by path (lightweight, no content).
    pub async fn get_metadata(&self, path: &str) -> Result<Option<PageSummary>> {
        match self.db.get(path).await? {
            Some(row) => Ok(Some(Self::row_to_summary(&row, row.content.len() as u64))),
            None => Ok(None),
        }
    }

    /// Ensure wiki directory structure exists.
    pub async fn init_dirs(&self) -> Result<()> {
        for dir in &[
            "entities/people",
            "entities/projects",
            "entities/concepts",
            "topics",
            "sources",
            "sops",
        ] {
            fs::create_dir_all(self.wiki_root.join(dir)).await?;
        }
        Ok(())
    }

    /// Read a page. Disk is SSOT for content; DB overlays runtime state.
    ///
    /// Resolution order:
    /// 1. Read `wiki_root/<path>.md` from disk → parse frontmatter + body.
    ///    Defaults for runtime fields (`access_count = 0`, `frequency = default`,
    ///    `created/updated = now`) are filled in by `from_markdown`.
    /// 2. If a matching DB row exists, overlay the runtime fields from it
    ///    (access_count, frequency, last_accessed, created, updated,
    ///    source_count, confidence). Content fields stay disk-fresh.
    /// 3. If disk file is missing, fall back to DB and surface a debug log —
    ///    that is a damaged-index state which `sync_db_from_disk` will fix.
    pub async fn read(&self, path: &str) -> Result<WikiPage> {
        let disk_path = self.wiki_root.join(format!("{}.md", path));
        match fs::read_to_string(&disk_path).await {
            Ok(markdown) => {
                let mut page = WikiPage::from_markdown(path.to_string(), &markdown)?;
                page.file_mtime = Self::file_mtime(&disk_path).await.unwrap_or(0);
                if let Some(row) = self.db.get(path).await? {
                    Self::overlay_runtime_state(&mut page, &row);
                }
                Ok(page)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(row) = self.db.get(path).await? {
                    tracing::debug!(
                        "PageStore::read('{}'): disk file missing, returning stale DB row \
                         (run sync_db_from_disk to repair)",
                        path
                    );
                    return Ok(Self::row_to_page(row));
                }
                Err(e.into())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Batch-read full pages from SQLite.
    pub async fn read_many(&self, paths: &[String]) -> Result<Vec<WikiPage>> {
        let rows = self.db.get_many(paths).await?;
        Ok(rows.into_iter().map(Self::row_to_page).collect())
    }

    /// Write page: disk is SSOT. Atomic write to disk first, then update SQLite.
    pub async fn write(&self, page: &WikiPage) -> Result<()> {
        self.sync_to_disk(page).await?;
        let disk_path = self.wiki_root.join(format!("{}.md", page.path));
        let mtime = Self::file_mtime(&disk_path).await.unwrap_or(0);
        self.upsert_db(page, mtime).await?;
        self.notify_wiki_changed(&page.path).await;
        Ok(())
    }

    /// Update SQLite index for a page that is already on disk.
    pub async fn index_page(&self, page: &WikiPage) -> Result<()> {
        let disk_path = self.wiki_root.join(format!("{}.md", page.path));
        let mtime = Self::file_mtime(&disk_path).await.unwrap_or(0);
        self.upsert_db(page, mtime).await
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        let disk_path = self.wiki_root.join(format!("{}.md", path));
        let _ = fs::remove_file(&disk_path).await;
        self.db.delete(path).await?;
        self.notify_wiki_changed(path).await;
        Ok(())
    }

    pub async fn exists(&self, path: &str) -> Result<bool> {
        self.db.exists(path).await
    }

    pub async fn list(&self, filter: PageFilter) -> Result<Vec<PageSummary>> {
        let rows = match &filter.page_type {
            Some(pt) => self.db.list_by_type(pt.as_str()).await?,
            None => self.db.list_all().await?,
        };
        Ok(rows
            .iter()
            .map(|r| Self::row_to_summary(r, r.content.len() as u64))
            .collect())
    }

    /// Batch-load lightweight page summaries for a set of paths.
    pub async fn read_summaries(&self, paths: &[String]) -> Result<Vec<PageSummary>> {
        let rows = self.db.get_summaries_by_paths(paths).await?;
        Ok(rows.into_iter().map(Self::summary_row_to_summary).collect())
    }

    /// Sync page to disk as markdown using atomic write (crash-safe).
    pub async fn sync_to_disk(&self, page: &WikiPage) -> Result<()> {
        let path = self.wiki_root.join(format!("{}.md", page.path));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        atomic_write(&path, page.to_markdown()).await?;
        Ok(())
    }

    /// Incremental sync SQLite index from disk using a `max(file_mtime)` watermark.
    ///
    /// Only files whose mtime is greater than the stored watermark are read and
    /// re-indexed. After syncing, the watermark is updated to the new global
    /// max mtime (excluding `index.md` and `log.md`).
    ///
    /// Returns the number of dirty pages that were actually re-indexed.
    pub async fn sync_db_from_disk(&self) -> Result<usize> {
        // 1. Load current watermark.
        let watermark = self
            .kv
            .read(WATERMARK_KEY)
            .await?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);

        let wiki_root = self.wiki_root.clone();
        let disk_entries = tokio::task::spawn_blocking(move || {
            let mut entries = std::collections::HashMap::new();
            Self::walk_disk_with_mtime(&wiki_root, &wiki_root, &mut entries)?;
            Ok::<_, anyhow::Error>(entries)
        })
        .await
        .map_err(|e| anyhow::anyhow!("disk walk panicked: {}", e))??;

        // 2. Remove stale DB records (files deleted on disk).
        let db_rows = self.db.list_all().await?;
        let mut deleted = 0usize;
        for row in &db_rows {
            if !disk_entries.contains_key(&row.path) {
                self.db.delete(&row.path).await?;
                deleted += 1;
            }
        }

        // 3. Incremental re-index: only files newer than watermark.
        let mut dirty = 0usize;
        for (path, mtime) in &disk_entries {
            if *mtime <= watermark {
                // Pruned — disk has not changed since last sync.
                continue;
            }

            let full_path = self.wiki_root.join(format!("{}.md", path));
            let markdown = match fs::read_to_string(&full_path).await {
                Ok(md) => md,
                Err(e) => {
                    tracing::warn!("sync_db_from_disk: failed to read '{}': {}", path, e);
                    continue;
                }
            };

            let mut page = match WikiPage::from_markdown(path.clone(), &markdown) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("sync_db_from_disk: failed to parse '{}': {}", path, e);
                    continue;
                }
            };
            page.file_mtime = *mtime;

            // Preserve machine runtime state from existing DB record if any.
            if let Some(old) = self.db.get(path).await? {
                page.frequency = Frequency::from_str_lossy(&old.frequency);
                page.access_count = old.access_count as u64;
                page.last_accessed = old
                    .last_accessed
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc));
            }

            self.upsert_db(&page, *mtime).await?;
            self.notify_wiki_changed(path).await;
            dirty += 1;
        }

        // 4. Mark index.md as needing rebuild if anything changed.
        if dirty > 0 || deleted > 0 {
            self.kv.write(INDEX_DIRTY_KEY, "true").await?;
        }

        // 5. Update watermark to max mtime (excluding meta files).
        let max_mtime = disk_entries
            .iter()
            .filter(|(path, _)| !EXCLUDED_FROM_WATERMARK.contains(&path.as_str()))
            .map(|(_, mtime)| *mtime)
            .max()
            .unwrap_or(watermark);
        self.kv.write(WATERMARK_KEY, &max_mtime.to_string()).await?;

        tracing::debug!(
            "sync_db_from_disk: dirty={}, deleted={}, watermark={}",
            dirty,
            deleted,
            max_mtime
        );
        Ok(dirty)
    }

    /// Rebuild `wiki/index.md` from the latest SQLite projection if the dirty
    /// flag is set. Returns `true` if a rebuild was performed.
    pub async fn maybe_rebuild_index_md(&self) -> Result<bool> {
        let is_dirty = self.kv.read(INDEX_DIRTY_KEY).await?.unwrap_or_default() == "true";
        if !is_dirty {
            return Ok(false);
        }

        let pages = self.list(Default::default()).await?;
        if pages.is_empty() {
            self.kv.write(INDEX_DIRTY_KEY, "false").await?;
            return Ok(false);
        }

        let mut md = String::from("---\ntitle: \"Wiki Index\"\ntype: topic\n---\n\n");
        md.push_str("# Wiki Index\n\n");

        // Group by type first, then by category.
        let mut by_type: std::collections::BTreeMap<String, Vec<&PageSummary>> =
            std::collections::BTreeMap::new();
        for page in &pages {
            by_type
                .entry(page.page_type.as_str().to_string())
                .or_default()
                .push(page);
        }

        for (type_name, type_pages) in &by_type {
            md.push_str(&format!("## {}\n\n", capitalize(type_name)));

            let mut by_category: std::collections::BTreeMap<String, Vec<&PageSummary>> =
                std::collections::BTreeMap::new();
            let mut uncategorized = Vec::new();

            for page in type_pages {
                if let Some(cat) = &page.category {
                    by_category.entry(cat.clone()).or_default().push(page);
                } else {
                    uncategorized.push(*page);
                }
            }

            for (cat, cat_pages) in &by_category {
                md.push_str(&format!("### {}\n\n", cat));
                for page in cat_pages {
                    md.push_str(&format!("- [[{}|{}]]\n", page.path, page.title));
                }
                md.push('\n');
            }

            if !uncategorized.is_empty() {
                md.push_str("### Uncategorized\n\n");
                for page in &uncategorized {
                    md.push_str(&format!("- [[{}|{}]]\n", page.path, page.title));
                }
                md.push('\n');
            }
        }

        let index_path = self.wiki_root.join("index.md");
        atomic_write(&index_path, md).await?;
        self.kv.write(INDEX_DIRTY_KEY, "false").await?;

        tracing::info!("Rebuilt wiki/index.md ({} pages)", pages.len());
        Ok(true)
    }

    /// Read the current sync watermark from kv_store.
    pub async fn kv_read_watermark(&self) -> Result<Option<String>> {
        self.kv.read(WATERMARK_KEY).await
    }

    // -- private helpers --

    fn parse_tags(tags: Option<&str>) -> Vec<String> {
        tags.and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or_default()
    }

    fn parse_rfc3339(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_default()
    }

    fn parse_optional_rfc3339(s: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
        s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
    }

    fn row_to_page(row: super::page_store::PageRow) -> WikiPage {
        WikiPage {
            path: row.path,
            title: row.title,
            page_type: row.page_type.parse().unwrap_or(PageType::Topic),
            category: row.category,
            tags: Self::parse_tags(row.tags.as_deref()),
            summary: row.summary,
            content: row.content,
            created: Self::parse_rfc3339(&row.created),
            updated: Self::parse_rfc3339(&row.updated),
            source_count: row.source_count as u32,
            confidence: row.confidence,
            frequency: Frequency::from_str_lossy(&row.frequency),
            access_count: row.access_count as u64,
            last_accessed: Self::parse_optional_rfc3339(row.last_accessed.as_deref()),
            file_mtime: row.file_mtime,
        }
    }

    /// Overlay DB runtime state onto a disk-loaded `WikiPage`.
    ///
    /// Disk supplies content fields (title/type/category/tags/summary/content);
    /// DB supplies the runtime/index fields that cannot live in the markdown
    /// frontmatter (timestamps, access stats, frequency, confidence).
    fn overlay_runtime_state(page: &mut WikiPage, row: &super::page_store::PageRow) {
        page.created = Self::parse_rfc3339(&row.created);
        page.updated = Self::parse_rfc3339(&row.updated);
        page.source_count = row.source_count as u32;
        page.confidence = row.confidence;
        page.frequency = Frequency::from_str_lossy(&row.frequency);
        page.access_count = row.access_count as u64;
        page.last_accessed = Self::parse_optional_rfc3339(row.last_accessed.as_deref());
    }

    fn row_to_summary(row: &super::page_store::PageRow, content_length: u64) -> PageSummary {
        PageSummary {
            path: row.path.clone(),
            title: row.title.clone(),
            page_type: row.page_type.parse().unwrap_or(PageType::Topic),
            category: row.category.clone(),
            tags: Self::parse_tags(row.tags.as_deref()),
            updated: Self::parse_rfc3339(&row.updated),
            confidence: row.confidence,
            frequency: Frequency::from_str_lossy(&row.frequency),
            access_count: row.access_count as u64,
            last_accessed: Self::parse_optional_rfc3339(row.last_accessed.as_deref()),
            summary: row.summary.clone(),
            content_length,
            file_mtime: row.file_mtime,
        }
    }

    fn summary_row_to_summary(row: super::page_store::PageSummaryRow) -> PageSummary {
        PageSummary {
            path: row.path,
            title: row.title,
            page_type: row.page_type.parse().unwrap_or(PageType::Topic),
            category: row.category,
            tags: Self::parse_tags(row.tags.as_deref()),
            updated: Self::parse_rfc3339(&row.updated),
            confidence: row.confidence,
            frequency: Frequency::from_str_lossy(&row.frequency),
            access_count: row.access_count as u64,
            last_accessed: Self::parse_optional_rfc3339(row.last_accessed.as_deref()),
            summary: row.summary,
            content_length: row.content_length as u64,
            file_mtime: row.file_mtime,
        }
    }

    async fn upsert_db(&self, page: &WikiPage, file_mtime: i64) -> Result<()> {
        let tags_str = serde_json::to_string(&page.tags)?;
        let checksum = Some(format!("{}", page.content.len()));
        self.db
            .upsert(&super::page_store::WikiPageInput {
                path: &page.path,
                title: &page.title,
                page_type: page.page_type.as_str(),
                category: page.category.as_deref(),
                tags: &tags_str,
                summary: page.summary.as_deref(),
                content: &page.content,
                source_count: page.source_count,
                confidence: page.confidence,
                checksum: checksum.as_deref(),
                frequency: page.frequency,
                access_count: page.access_count,
                last_accessed: page.last_accessed.map(|dt| dt.to_rfc3339()),
                file_mtime,
            })
            .await?;
        Ok(())
    }

    /// Publish a non-blocking `WikiChanged` notification.
    async fn notify_wiki_changed(&self, path: &str) {
        if let Some(ref tx) = self.wiki_changed_tx {
            if let Err(e) = tx.try_send(path.to_string()) {
                tracing::debug!("PageStore: failed to send WikiChanged for {}: {}", path, e);
            }
        }
    }

    /// Get file modification time as Unix epoch seconds.
    pub async fn file_mtime(path: &PathBuf) -> Result<i64> {
        let meta = fs::metadata(path).await?;
        let modified = meta.modified()?;
        let secs = modified.duration_since(UNIX_EPOCH)?.as_secs() as i64;
        Ok(secs)
    }

    fn walk_disk_with_mtime(
        root: &PathBuf,
        dir: &PathBuf,
        out: &mut std::collections::HashMap<String, i64>,
    ) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                Self::walk_disk_with_mtime(root, &path, out)?;
            } else if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
                let rel = path.strip_prefix(root)?;
                let rel_str = {
                    let s = rel.to_string_lossy();
                    s.strip_suffix(".md").unwrap_or(&s).to_string()
                };
                let mtime = match std::fs::metadata(&path)?.modified() {
                    Ok(t) => t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64,
                    Err(_) => 0,
                };
                out.insert(rel_str, mtime);
            }
        }
        Ok(())
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn temp_page_store() -> (PageStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS wiki_pages (
                path TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                type TEXT NOT NULL,
                category TEXT,
                tags TEXT,
                summary TEXT,
                content TEXT NOT NULL,
                created TEXT NOT NULL,
                updated TEXT NOT NULL,
                source_count INTEGER NOT NULL DEFAULT 0,
                confidence REAL NOT NULL DEFAULT 1.0,
                checksum TEXT,
                frequency TEXT NOT NULL DEFAULT 'archived',
                access_count INTEGER NOT NULL DEFAULT 0,
                last_accessed TEXT,
                file_mtime INTEGER NOT NULL DEFAULT 0
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS kv_store (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let wiki_root = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki_root).unwrap();
        let store = PageStore::new(pool, wiki_root);
        (store, dir)
    }

    fn write_md(root: &PathBuf, path: &str, content: &str) {
        let full = root.join(format!("{}.md", path));
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, content).unwrap();
    }

    #[tokio::test]
    async fn test_incremental_sync_skips_unchanged_files() {
        let (store, _dir) = temp_page_store().await;
        let root = store.wiki_root().clone();

        // Create two files.
        write_md(
            &root,
            "topics/rust",
            "---\ntitle: \"Rust\"\ntype: topic\n---\n\nRust lang",
        );
        write_md(
            &root,
            "topics/go",
            "---\ntitle: \"Go\"\ntype: topic\n---\n\nGo lang",
        );

        // First sync — full indexing.
        let synced1 = store.sync_db_from_disk().await.unwrap();
        assert_eq!(synced1, 2);

        // Second sync — nothing changed, should prune both.
        let synced2 = store.sync_db_from_disk().await.unwrap();
        assert_eq!(synced2, 0);

        // Modify only rust.md.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        write_md(
            &root,
            "topics/rust",
            "---\ntitle: \"Rust\"\ntype: topic\n---\n\nRust language updated",
        );

        // Third sync — only rust should be re-indexed.
        let synced3 = store.sync_db_from_disk().await.unwrap();
        assert_eq!(synced3, 1);

        // Verify DB content.
        let page = store.read("topics/rust").await.unwrap();
        assert!(page.content.contains("updated"));
        let go = store.read("topics/go").await.unwrap();
        assert_eq!(go.content, "Go lang");
    }

    #[tokio::test]
    async fn test_sync_removes_deleted_files() {
        let (store, _dir) = temp_page_store().await;
        let root = store.wiki_root().clone();

        write_md(
            &root,
            "topics/old",
            "---\ntitle: \"Old\"\ntype: topic\n---\n\nold",
        );
        store.sync_db_from_disk().await.unwrap();
        assert!(store.exists("topics/old").await.unwrap());

        // Delete file on disk.
        std::fs::remove_file(root.join("topics/old.md")).unwrap();
        let synced = store.sync_db_from_disk().await.unwrap();
        // deleted counts toward dirty but method returns dirty count (0 here since no new files).
        assert_eq!(synced, 0);
        assert!(!store.exists("topics/old").await.unwrap());
    }

    #[tokio::test]
    async fn test_watermark_excludes_index_and_log() {
        let (store, _dir) = temp_page_store().await;
        let root = store.wiki_root().clone();

        write_md(&root, "topics/a", "---\ntitle: A\ntype: topic\n---\n\na");
        store.sync_db_from_disk().await.unwrap();

        let wm1 = store.kv_read_watermark().await.unwrap().unwrap();

        // Modify index.md — should NOT advance watermark.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        write_md(
            &root,
            "index",
            "---\ntitle: Index\ntype: topic\n---\n\nupdated",
        );
        store.sync_db_from_disk().await.unwrap();

        let wm2 = store.kv_read_watermark().await.unwrap().unwrap();
        assert_eq!(wm1, wm2);
    }

    #[tokio::test]
    async fn test_rebuild_index_md() {
        let (store, _dir) = temp_page_store().await;
        let root = store.wiki_root().clone();

        write_md(
            &root,
            "topics/rust",
            "---\ntitle: \"Rust\"\ntype: topic\ncategory: \"Languages\"\n---\n\nRust",
        );
        write_md(
            &root,
            "topics/go",
            "---\ntitle: \"Go\"\ntype: topic\ncategory: \"Languages\"\n---\n\nGo",
        );
        store.sync_db_from_disk().await.unwrap();

        // Dirty flag should be set.
        let rebuilt = store.maybe_rebuild_index_md().await.unwrap();
        assert!(rebuilt);

        // Verify index.md was written.
        let index = fs::read_to_string(root.join("index.md")).await.unwrap();
        assert!(index.contains("Rust"));
        assert!(index.contains("Go"));
        assert!(index.contains("Languages"));

        // Second call should be no-op.
        let rebuilt2 = store.maybe_rebuild_index_md().await.unwrap();
        assert!(!rebuilt2);
    }
}
