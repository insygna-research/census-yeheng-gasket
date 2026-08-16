# Session 全文检索(FTS5 sidecar 索引)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**YAGNI 门禁(先读再决定做不做):** 本功能是 P2、**门控**项。现状 REST 已能 `GET /api/sessions/{key}/messages` 拉单会话全文由前端过滤;只有当「会话数量多到逐会话翻找成为真实痛点」时才值得实现。这份计划的存在不是为了现在动手,而是**把设计提前定稿**,避免将来带着痛点现场拍脑袋。动手前先回答:现在到底有多少个 session?个位数就关闭本计划。**门禁毙掉 FTS5 时的退路:**免依赖的线性子串扫描,可直接放 gasket-host、零 feature 零 rusqlite —— 两个主机免费得到;只有 FTS5 才付 feature 税。

**Goal:** 跨会话全文检索,两个消费方共用一个引擎:网关 REST `GET /api/sessions/search?q=...` 与桌面端 Tauri command `search_sessions`,均以共享 `SessionHit` 形状返回,前端经单一适配器 `searchSessions` 无感切换。引擎用 SQLite FTS5 sidecar 索引对所有会话的 `events.jsonl` 做增量检索。

**Architecture:** 引擎住在 **gasket-host**(新文件 `gasket-host/src/session_index.rs`,经 `#[cfg(feature = "session-index")]` 门控,rusqlite 为可选依赖)—— 桌面端(`web/src-tauri`,进程内 host、无 REST 层)因此能用同一份代码,网关只是它的第一个消费者。sidecar 库在 `<config_dir>/index.db`:每条含文本的 SessionEvent(User/Assistant/ToolResult 的 Text 块)一行;`meta` 表按 session 记高水位 seq,reindex 只追加新事件。共享结果类型 `SessionHit { session_id, name, snippet }` 在引擎模块定义一次,网关 REST JSON 与 Tauri command 返回序列化完全一致。索引按需构建:网关在首个搜索请求内联 `await reindex`(OnceCell 门闩放 `AppState`,无后台线程、不碰写路径);桌面端 command 每次调用无状态地开 Connection、跑高水位增量 reindex 检查、查询、返回(无全局注册表、无缓存 —— 资源状态属于 host,不做进程级 static)。事件读取复用 `EventStorage::load_events`(`gasket-core/src/storage/mod.rs` ~line 523);列会话用 `read_dir(store_root)` + `is_valid_session_id`。**设计修正一处:最初草图写 `reindex(store: &JsonlStorage)`,但 `events.jsonl` 的读取方是 `EventStorage`(`JsonlStorage` 只管 legacy `messages.jsonl`),故签名定为 `reindex(store_root: &Path, db_path: &Path)`。**

**Tech Stack:** rusqlite(features=["bundled"],optional,仅 gasket-host,由 feature `session-index` 引入;网关/桌面端经 gasket-host feature 传递,自身不加 rusqlite)、既有 EventStorage/SessionEvent、axum Query 提取器、Tauri command。

## Global Constraints

- 新依赖只加 `gasket-host/Cargo.toml`:`rusqlite = { version = "0.32", features = ["bundled"], optional = true }` 与 `anyhow = { workspace = true, optional = true }`,加 `[features] session-index = ["dep:rusqlite", "dep:anyhow"]`,默认关闭(bundled 自带 SQLite 含 FTS5;版本以实现日 crates.io 最新稳定为准,features 不变)。hosting 风格对齐 gasket-ext 的 `terminal` feature(可选依赖 + cfg 门控模块,默认零影响)。**gasket-gateway 自身不加、也不保留 rusqlite 依赖**;两个消费方 opt-in:网关 `gasket-host = { workspace = true, features = ["session-index"] }`,桌面端 `web/src-tauri` 的 gasket-host 依赖同样加 `features = ["session-index"]`。
- CLI 不接入(feature 默认关闭;日后可选 `/search` 命令 opt-in —— future,不是本计划任务)。
- 引擎单元测试全部住 `gasket-host/src/session_index.rs` 的 `#[cfg(test)] mod tests`(随 feature 编译,`--all-features` 下运行);gasket-gateway 只留 endpoint 接线测试。**`cargo test -p gasket-host`(不带 feature)必须通过且模块被 cfg 掉、不参与编译** —— 每个引擎 task 的验证都含这一条。
- sidecar 路径:生产 `gasket_core::storage::config_dir().join("index.db")`;测试经 `AppState` 注入 tempdir。
- schema 固定:`CREATE VIRTUAL TABLE events USING fts5(session_id UNINDEXED, seq UNINDEXED, kind UNINDEXED, text)` + `CREATE TABLE meta(key TEXT PRIMARY KEY, value INTEGER NOT NULL)`。
- `seq` = 事件在日志中的 0-based 序号(不是行计数),保证跳过 TurnStart/TurnEnd 后高水位仍单调正确。
- 用户查询整体短语引号包裹(内部 `"` 翻倍转义),杜绝 FTS5 语法注入/解析报错。
- 错误语义对齐既有路由(api.rs `get_messages` ~line 126):索引/存储故障 → 500(fail loud);无命中是合法空列表 → 200 `{ "hits": [] }`,不是 404。桌面端对位:command 返回 `Err(String)` 对应 500 语义(fail loud),空命中返回空 `Vec`。
- 索引构建在首个搜索请求内联完成(同 api.rs 现有内联磁盘 I/O 风格;桌面端为每次调用内联,高水位保证增量幂等);无后台线程、无写路径埋点。
- rustfmt:4 空格、max_width 100;CI:`cargo fmt --check`、`cargo clippy --all-features --all-targets -D warnings`、`cargo test --all-features`(注意:`--all-features` 会启用 `session-index`,引擎测试随 CI 运行)。

---

### Task 1: schema + `init_db`(gasket-host,feature 骨架)

**Files:**
- Create: `gasket/gasket-host/src/session_index.rs`
- Modify: `gasket/gasket-host/Cargo.toml`([dependencies] 表尾 `reqwest` 之后;新增 `[features]` 段)
- Modify: `gasket/gasket-host/src/lib.rs`(`pub mod session;` 之后插 `#[cfg(feature = "session-index")] pub mod session_index;`)
- Test: `session_index.rs` 内 `#[cfg(test)] mod tests`(随 feature 编译)

**Interfaces:**
- Produces: `pub fn init_db(db_path: &Path) -> anyhow::Result<rusqlite::Connection>`(幂等;跨 crate 供网关/桌面端复用,故 `pub` 而非 `pub(crate)`)
- Produces: `pub struct SessionHit { session_id: String, name: Option<String>, snippet: String }`(`#[derive(Debug, serde::Serialize)]`;两个消费方共用的 wire 形状,在引擎模块定义一次)
- Produces: `pub struct Stats { sessions: usize, events_indexed: usize }`(`Default`/`PartialEq`)

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_db_creates_fts5_and_meta_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        let conn = init_db(&db).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('events', 'meta')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "FTS5 table and meta table must both exist");
        assert!(init_db(&db).is_ok(), "second open is idempotent");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-host --features session-index
# expected: compile error — session_index / init_db 不存在
```

- [ ] **Step 3: Minimal implementation**

`gasket/gasket-host/Cargo.toml` 增补(保留既有条目;风格对齐 gasket-ext `terminal` feature):

```toml
[dependencies]
# ... 既有 gasket-core / dotenvy / thiserror / parking_lot / tracing / uuid / tokio / serde / serde_json / reqwest ...
rusqlite = { version = "0.32", features = ["bundled"], optional = true }
anyhow = { workspace = true, optional = true }

[features]
session-index = ["dep:rusqlite", "dep:anyhow"]
```

`gasket/gasket-host/src/lib.rs` 模块声明(`pub mod session;` 之后):

```rust
#[cfg(feature = "session-index")]
pub mod session_index;
```

`session_index.rs`(命名避开 gasket-ext 已有的 `search.rs`,那是 web_search):

```rust
//! Session full-text search: an FTS5 sidecar index over the on-disk event
//! logs. Lives in gasket-host behind Cargo feature `session-index`; the
//! gateway REST route and the desktop Tauri command are the two consumers.
//!
//! One SQLite database at `<config_dir>/index.db`. Every text-bearing
//! SessionEvent becomes one row; a per-session high-water mark in `meta`
//! keeps reindexing incremental. Built lazily on demand — no background
//! thread, write path untouched.

use std::path::Path;

use gasket_core::types::message::AgentMessage;
use gasket_core::{EventStorage, SessionEvent};
use rusqlite::Connection;

/// Shared hit shape for both consumers (gateway REST JSON and the desktop
/// Tauri command serialize this identically). `name` comes from the
/// session's `meta.json` sidecar; `snippet` is the FTS5 snippet.
#[derive(Debug, serde::Serialize)]
pub struct SessionHit {
    pub session_id: String,
    pub name: Option<String>,
    pub snippet: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Stats {
    /// Sessions that had new rows inserted this run.
    pub sessions: usize,
    pub events_indexed: usize,
}

/// Open (creating if needed) the sidecar index and ensure the schema.
pub fn init_db(db_path: &Path) -> anyhow::Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(db_path)?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS events USING fts5(\
             session_id UNINDEXED, seq UNINDEXED, kind UNINDEXED, text);\
         CREATE TABLE IF NOT EXISTS meta(\
             key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
    )?;
    Ok(conn)
}
```

- [ ] **Step 4: Run to verify it passes**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-host --features session-index   # expected: 1 passed
cargo test -p gasket-host   # feature off:模块被 cfg 掉、不参与编译,既有套件保持绿
```

- [ ] **Step 5: Commit**

```bash
git add gasket/gasket-host/Cargo.toml gasket/gasket-host/src/session_index.rs gasket/gasket-host/src/lib.rs
git commit -m "feat(host): FTS5 sidecar index schema and db init (feature \"session-index\")"
```

---

### Task 2: events → rows 提取(复用 SessionEvent 形状)

**Files:**
- Modify: `gasket/gasket-host/src/session_index.rs`(追加 `block_text`/`Row`/`event_rows`)
- Test: 同文件

**Interfaces:**
- Produces: `fn event_rows(events: &[SessionEvent]) -> Vec<Row>`(`Row { seq: usize, kind: &'static str, text: String }`,模块私有)
- Consumes: `gasket_core::SessionEvent`(`gasket-core/src/types/session_event.rs` ~line 15)的 User/Assistant/ToolResult 变体

- [ ] **Step 1: Write the failing test**

```rust
    use gasket_core::types::session_event::TurnEndReason;

    fn user_ev(t: &str) -> SessionEvent {
        SessionEvent::User(AgentMessage::user(t))
    }

    #[tokio::test]
    async fn event_rows_extracts_text_and_skips_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let events = vec![
            SessionEvent::TurnStart,
            user_ev("find the flaky test"),
            SessionEvent::TurnEnd { reason: TurnEndReason::Completed },
        ];
        store.append_events("s1", &events).await.unwrap();
        let rows = event_rows(&store.load_events("s1").await.unwrap());
        assert_eq!(rows.len(), 1, "markers produce no rows");
        assert_eq!(rows[0].seq, 1, "seq is the log index, not the row index");
        assert_eq!(rows[0].kind, "user");
        assert!(rows[0].text.contains("flaky"));
    }
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-host --features session-index event_rows
# expected: compile error — event_rows 未定义
```

- [ ] **Step 3: Minimal implementation**

```rust
struct Row {
    seq: usize,
    kind: &'static str,
    text: String,
}

fn block_text(content: &[gasket_core::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            gasket_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Project one session's event log into indexable rows. `seq` is the event's
/// index in the log (0-based), so the high-water mark stays monotonic even
/// though marker events produce no row.
fn event_rows(events: &[SessionEvent]) -> Vec<Row> {
    events
        .iter()
        .enumerate()
        .filter_map(|(seq, ev)| {
            let (kind, msg): (&'static str, &AgentMessage) = match ev {
                SessionEvent::User(m) => ("user", m),
                SessionEvent::Assistant { message: m, .. } => ("assistant", m),
                SessionEvent::ToolResult(m) => ("tool_result", m),
                SessionEvent::TurnStart | SessionEvent::TurnEnd { .. } => return None,
            };
            let content = match msg {
                AgentMessage::User(u) => block_text(&u.content),
                AgentMessage::Assistant(a) => block_text(&a.content),
                AgentMessage::ToolResult(t) => block_text(&t.content),
                AgentMessage::Custom(_) => return None,
            };
            if content.is_empty() { return None; }
            Some(Row { seq, kind, text: content })
        })
        .collect()
}
```

- [ ] **Step 4: Run to verify it passes**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-host --features session-index   # expected: 2 passed
```

- [ ] **Step 5: Commit**

```bash
git add gasket/gasket-host/src/session_index.rs && git commit -m "feat(host): project session events into indexable rows"
```

---

### Task 3: 高水位增量 reindex

**Files:**
- Modify: `gasket/gasket-host/src/session_index.rs`(追加 `reindex`)
- Test: 同文件

**Interfaces:**
- Produces: `pub async fn reindex(store_root: &Path, db_path: &Path) -> anyhow::Result<Stats>`(跨 crate 供网关/桌面端调用)
- Consumes: Task 1 `init_db`、Task 2 `event_rows`、`EventStorage::load_events`、`gasket_core::is_valid_session_id`

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn reindex_is_incremental_across_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let db = tmp.path().join("index.db");
        store
            .append_events("s1", &[SessionEvent::TurnStart, user_ev("first message")])
            .await
            .unwrap();
        let first = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!((first.sessions, first.events_indexed), (1, 1));

        store
            .append_events(
                "s1",
                &[
                    user_ev("second message"),
                    SessionEvent::TurnEnd { reason: TurnEndReason::Completed },
                ],
            )
            .await
            .unwrap();
        let second = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!(second.events_indexed, 1, "only the newly appended text event");

        let conn = init_db(&db).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "no duplicate rows");
        let mark: i64 = conn
            .query_row("SELECT value FROM meta WHERE key = 's1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mark, 2, "high-water mark is the max indexed seq (0-based)");
    }

    #[tokio::test]
    async fn reindex_on_empty_root_is_zero_stats() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(reindex(tmp.path(), &tmp.path().join("index.db")).await.unwrap(), Stats::default());
    }
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-host --features session-index reindex
# expected: compile error — reindex 未定义
```

- [ ] **Step 3: Minimal implementation**

```rust
/// Incremental reindex: for every session dir under `store_root`, append
/// only events past the per-session high-water mark stored in `meta`.
pub async fn reindex(store_root: &Path, db_path: &Path) -> anyhow::Result<Stats> {
    let conn = init_db(db_path)?;
    let storage = EventStorage::new(store_root);
    let mut stats = Stats::default();
    let mut ids: Vec<String> = std::fs::read_dir(store_root)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|id| gasket_core::is_valid_session_id(id))
        .collect();
    ids.sort(); // deterministic order for tests and logging
    for id in ids {
        let events = storage.load_events(&id).await?;
        let last: i64 = {
            let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
            stmt.query_row([&id], |r| r.get(0)).unwrap_or(0)
        };
        let mut inserted = 0usize;
        let mut max_seq = last;
        {
            let mut stmt = conn.prepare(
                "INSERT INTO events(session_id, seq, kind, text) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for row in event_rows(&events) {
                if (row.seq as i64) <= last { continue; }
                stmt.execute(rusqlite::params![id, row.seq as i64, row.kind, row.text])?;
                max_seq = max_seq.max(row.seq as i64);
                inserted += 1;
            }
        }
        if inserted > 0 {
            conn.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![id, max_seq],
            )?;
            stats.sessions += 1;
            stats.events_indexed += inserted;
        }
    }
    Ok(stats)
}
```

- [ ] **Step 4: Run to verify it passes**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-host --features session-index   # expected: 4 passed
```

- [ ] **Step 5: Commit**

```bash
git add gasket/gasket-host/src/session_index.rs && git commit -m "feat(host): incremental high-water-mark reindex over event logs"
```

---

### Task 4: FTS5 查询(`search` → 共享 `SessionHit`)

**Files:**
- Modify: `gasket/gasket-host/src/session_index.rs`(追加 `search`)
- Test: 同文件

**Interfaces:**
- Produces: `pub async fn search(store_root: &Path, db_path: &Path, q: &str, limit: usize) -> anyhow::Result<Vec<SessionHit>>`(比旧草图多收 `store_root`:`name` 需读 meta.json sidecar;两个消费方共用同一入口,wire 形状只在此定义一次)

- [ ] **Step 1: Write the failing test**

```rust
    use gasket_core::SessionMeta;

    #[tokio::test]
    async fn search_returns_snippet_names_phrase_quoting_and_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let db = tmp.path().join("index.db");
        store.append_events("s1", &[user_ev("the flaky test failed again")]).await.unwrap();
        store.append_events("s2", &[user_ev("NEAR(a b) is fts5 syntax")]).await.unwrap();
        store.append_events("s3", &[user_ev("needle one")]).await.unwrap();
        store.append_events("s4", &[user_ev("needle two")]).await.unwrap();
        store.append_events("s5", &[user_ev("needle three")]).await.unwrap();
        store.write_meta("s1", &SessionMeta { name: Some("flaky hunt".into()) }).await.unwrap();
        reindex(tmp.path(), &db).await.unwrap();

        let hits = search(tmp.path(), &db, "flaky", 20).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        assert_eq!(hits[0].name.as_deref(), Some("flaky hunt"), "name enriched from meta.json");
        assert!(hits[0].snippet.contains("flaky"));
        assert!(search(tmp.path(), &db, "zebra", 20).await.unwrap().is_empty(), "no hit is empty, not error");
        assert!(search(tmp.path(), &db, "NEAR(a b", 20).await.is_ok(), "syntax-looking input never parsed as FTS5");
        assert_eq!(search(tmp.path(), &db, "needle", 2).await.unwrap().len(), 2, "limit respected");
    }
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-host --features session-index search_returns
# expected: compile error — search 未定义
```

- [ ] **Step 3: Minimal implementation**

```rust
/// FTS5 MATCH search over the index. The query is phrase-quoted (inner
/// double quotes doubled) so user input can never be parsed as FTS5
/// syntax. Rows map to the shared `SessionHit`; ordering is bm25 rank,
/// `name` is enriched from the session's meta.json sidecar.
pub async fn search(
    store_root: &Path,
    db_path: &Path,
    q: &str,
    limit: usize,
) -> anyhow::Result<Vec<SessionHit>> {
    let conn = init_db(db_path)?;
    let phrase = format!("\"{}\"", q.replace('"', "\"\""));
    let mut stmt = conn.prepare(
        "SELECT session_id, snippet(events, 3, '', '', '…', 16), rank \
         FROM events WHERE events MATCH ?1 ORDER BY rank LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![phrase, limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let storage = EventStorage::new(store_root);
    let mut hits = Vec::with_capacity(rows.len());
    for (session_id, snippet) in rows {
        let name = storage.load_meta(&session_id).await.and_then(|m| m.name);
        hits.push(SessionHit { session_id, name, snippet });
    }
    Ok(hits)
}
```

- [ ] **Step 4: Run to verify it passes**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-host --features session-index   # expected: 5 passed
cargo test -p gasket-host && cargo clippy --all-features --all-targets -D warnings && cargo fmt --check   # feature off 全绿 + 全量门禁
```

- [ ] **Step 5: Commit**

```bash
git add gasket/gasket-host/src/session_index.rs && git commit -m "feat(host): phrase-quoted FTS5 search returning shared SessionHit"
```

---

### Task 5: REST 路由接线(消费方 1:gateway)+ AppState + 文档

**Files:**
- Modify: `gasket/gasket-gateway/Cargo.toml`(gasket-host 依赖行加 `features = ["session-index"]`;网关自身**不加** rusqlite —— 引擎在 host,依赖经 feature 传递)
- Modify: `gasket/gasket-gateway/src/state.rs`(`AppState` ~line 13-19 追加 2 字段)
- Modify: `gasket/gasket-gateway/src/main.rs`(AppState 构造 ~line 64-67;路由表 ~line 71-79;handler import ~line 45-48)
- Modify: `gasket/gasket-gateway/src/api.rs`(新增 `SearchParams` + `search_sessions`;测试 `test_state`/`api_router` ~line 277-291)
- Modify: `docs/usage.md`(§4.1 路由清单行 ~line 114)
- Test: `api.rs` 内 `#[cfg(test)] mod tests`(仅 endpoint 接线;引擎行为测试全部在 gasket-host)

**Interfaces:**
- Consumes: Task 3 `gasket_host::session_index::reindex`、Task 4 `search`/`SessionHit`
- Produces: `GET /api/sessions/search?q=<phrase>&limit=<n>` → `200 { "hits": [ { session_id, name, snippet }, ... ] }`(与桌面端 command 返回同形,序列化同源);空 q → 400;索引/存储故障 → 500
- Produces: `AppState { ..., index_db: PathBuf, search_ready: tokio::sync::OnceCell<anyhow::Result<()>> }`(reindex 门闩放 state 而非进程级 static,测试各自注入 tempdir 互不串扰;reindex-on-first-request 是网关侧策略,桌面端不走此门闩)

- [ ] **Step 1: Write the failing test**

```rust
    fn test_state(root: std::path::PathBuf) -> Arc<AppState> {
        Arc::new(AppState {
            sessions: DashMap::new(),
            store_root: root.clone(),
            index_db: root.join("index.db"),
            search_ready: tokio::sync::OnceCell::new(),
        })
    }

    fn api_router(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/api/sessions", get(list_sessions))
            .route("/api/sessions/search", get(search_sessions))
            .route("/api/sessions/{key}/messages", get(get_messages))
            .route("/api/sessions/{key}/name", put(rename_session))
            .route("/api/sessions/{key}", delete(delete_session))
            .with_state(state)
    }

    #[tokio::test]
    async fn search_route_returns_hits_and_rejects_blank_q() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = EventStorage::new(tmp.path().to_path_buf());
        storage.append_event("sess-1", &user_event("the flaky test")).await.unwrap();
        let app = api_router(test_state(tmp.path().to_path_buf()));
        let res = app.clone().oneshot(get_uri("/api/sessions/search?q=flaky")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["hits"][0]["session_id"], "sess-1");
        assert!(v["hits"][0]["snippet"].as_str().unwrap().contains("flaky"));
        assert!(v["hits"][0]["name"].is_null(), "unnamed session serializes name as null");
        let res = app.oneshot(get_uri("/api/sessions/search?q=%20")).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-gateway search_route
# expected: compile error — AppState 缺字段 / search_sessions 未定义
```

- [ ] **Step 3: Minimal implementation**

`gasket/gasket-gateway/Cargo.toml` — gasket-host 依赖行改为(引擎经 feature 进入,网关不加自己的 rusqlite):

```toml
gasket-host = { workspace = true, features = ["session-index"] }
```

`state.rs` `AppState` 追加:

```rust
    /// FTS5 sidecar index (production: `~/.gasket/index.db`; tests inject
    /// a tempdir path).
    pub(crate) index_db: PathBuf,
    /// Reindex-on-demand latch: the first search request per process (per
    /// state, in tests) populates the index; later requests reuse it.
    pub(crate) search_ready: tokio::sync::OnceCell<anyhow::Result<()>>,
```

`main.rs` — AppState 构造、路由(`{key}` 路由之前)、import 列表加 `search_sessions`:

```rust
    let state = Arc::new(AppState {
        sessions: DashMap::new(),
        store_root: gasket_core::JsonlStorage::default_root().base_dir_clone(),
        index_db: gasket_core::storage::config_dir().join("index.db"),
        search_ready: tokio::sync::OnceCell::new(),
    });
```

```rust
        .route("/api/sessions/search", get(search_sessions))
```

(静态段 `/api/sessions/search` 与 `DELETE /api/sessions/{key}` 不冲突:matchit 静态段优先且方法不同。)

`api.rs` 新增:

```rust
#[derive(serde::Deserialize)]
pub(crate) struct SearchParams {
    q: String,
    limit: Option<usize>,
}

/// Full-text search across all sessions' event logs. The first request per
/// process builds/updates the FTS5 sidecar index (reindex-on-demand, then
/// latched); a store/index failure is a 500 (fail loud, same policy as
/// `get_messages`). No hits is a legitimate empty list — not a 404.
/// The engine itself lives in `gasket_host::session_index` (feature
/// `session-index`); the gateway is only transport.
pub(crate) async fn search_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<SearchParams>,
) -> Response {
    let q = params.q.trim().to_string();
    if q.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "q must be non-empty" })),
        )
            .into_response();
    }
    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let init = state
        .search_ready
        .get_or_init(|| async {
            gasket_host::session_index::reindex(&state.store_root, &state.index_db)
                .await
                .map(|_| ())
        })
        .await;
    if let Err(e) = init {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("index build failed: {e}") })),
        )
            .into_response();
    }
    match gasket_host::session_index::search(&state.store_root, &state.index_db, &q, limit).await {
        Ok(hits) => Json(json!({ "hits": hits })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
```

`docs/usage.md` §4.1 路由清单行(~line 114)在 `/api/sessions` 后插入:

```text
`GET /api/sessions/search?q=…`(FTS5 跨会话全文检索;每进程首个请求先增量重建 `~/.gasket/index.db` 索引)
```

- [ ] **Step 4: Run to verify it passes**

```bash
cd /Users/yeheng/workspaces/Github/gasket/gasket && cargo test -p gasket-gateway    # expected: all passed
cargo fmt --check && cargo clippy --all-features --all-targets -D warnings          # expected: clean
```

- [ ] **Step 5: Commit**

```bash
git add gasket/gasket-gateway/Cargo.toml gasket/gasket-gateway/src/state.rs gasket/gasket-gateway/src/main.rs gasket/gasket-gateway/src/api.rs docs/usage.md
git commit -m "feat(gateway): GET /api/sessions/search wired to gasket-host session_index"
```

---

### Task 6: 桌面端接线(消费方 2:Tauri command)

桌面端是进程内 host、无 REST 层 —— 引擎住 gasket-host 后它才能用上同一份代码,这正是本次迁移的动因。

**Files:**
- Modify: `web/src-tauri/Cargo.toml`(gasket-host 依赖行 ~line 31 加 `features = ["session-index"]`)
- Modify: `web/src-tauri/src/lib.rs`(`rename_session` 之后新增 `search_sessions`;`invoke_handler` 列表同步注册)

**Interfaces:**
- Consumes: `gasket_host::session_index::{reindex, search, SessionHit}`(Task 3/4)
- Produces: Tauri command `search_sessions(query: String) -> Result<Vec<SessionHit>, String>` —— **每次调用无状态**:开 `<config_dir>/index.db` 连接、跑高水位增量 reindex 检查、查询、返回。无全局注册表、无缓存状态(资源状态属于 host,不做进程级 static)。空 query → `Err`(对齐网关 400);索引/存储故障 → `Err`(对齐网关 500,fail loud);无命中 → 空 `Vec`
- Produces: 返回经 serde 序列化的 `SessionHit`,与网关 REST JSON 同形(类型同源,序列化同源)

- [ ] **Step 1: Minimal implementation**

`web/src-tauri/Cargo.toml`:

```toml
gasket-host = { path = "../../gasket/gasket-host", features = ["session-index"] }
```

`web/src-tauri/src/lib.rs`(`rename_session` 之后):

```rust
/// Cross-session full-text search (FTS5 sidecar at `~/.gasket/index.db`).
/// Stateless per call: open the connection, run the high-water incremental
/// reindex check, run the query, return hits. No registry, no cached
/// state — resource state belongs to the host, not process globals.
#[tauri::command]
async fn search_sessions(
  query: String,
) -> Result<Vec<gasket_host::session_index::SessionHit>, String> {
  let q = query.trim().to_string();
  if q.is_empty() {
    return Err("query must be non-empty".into());
  }
  let root = gasket_core::JsonlStorage::default_root().base_dir_clone();
  let db = gasket_core::storage::config_dir().join("index.db");
  gasket_host::session_index::reindex(&root, &db)
    .await
    .map_err(|e| e.to_string())?;
  gasket_host::session_index::search(&root, &db, &q, 20)
    .await
    .map_err(|e| e.to_string())
}
```

`invoke_handler`(`rename_session,` 之后插一行):

```rust
      search_sessions,
```

- [ ] **Step 2: Compile check(接线任务,无独立单测;引擎行为已由 Task 1-4 覆盖)**

```bash
cd /Users/yeheng/workspaces/Github/gasket/web/src-tauri && cargo check   # desktop opt-in 可编译(repo 无独立 pnpm tauri check script,Rust 侧编译检查即此步)
```

- [ ] **Step 3: Commit**

```bash
cd /Users/yeheng/workspaces/Github/gasket
git add web/src-tauri/Cargo.toml web/src-tauri/src/lib.rs
git commit -m "feat(desktop): search_sessions Tauri command over gasket-host session_index"
```

---

### Task 7: 前端适配器 `searchSessions`(浏览器/桌面同形)

**Files:**
- Modify: `web/src/lib/backend.ts`(追加 `SessionHit` 接口 + `searchSessions`)

**Interfaces:**
- Produces: `searchSessions(q: string): Promise<SessionHit[]>`;`SessionHit { session_id: string; name?: string | null; snippet: string }`(TS 形状对齐 Task 4 的 Rust wire 类型)
- Tauri 模式 `invoke('search_sessions', { query: q })`;浏览器模式 `fetch(`${backendBaseUrl()}/api/sessions/search?q=${encodeURIComponent(q)}`)`。isTauri 分支模式与本文件既有命令及 `NetworkProxyDialog.vue` 的 save 路径一致;`VITE_API_URL` 已在 `web/.env.example` 记录

- [ ] **Step 1: Minimal implementation**(`backend.ts` 末尾追加;纯接线,TS 侧以类型检查为门)

```ts
/** Cross-session full-text search hit — the gateway REST route and the
 * desktop Tauri command return the same shape (single engine, single
 * SessionHit definition in gasket-host). */
export interface SessionHit {
  session_id: string;
  /** Display name from the session's meta sidecar; null when unnamed. */
  name?: string | null;
  snippet: string;
}

/** Cross-session full-text search. Empty list on failure or no hits —
 * callers render an empty result, not an error toast. */
export async function searchSessions(q: string): Promise<SessionHit[]> {
  try {
    if (isTauri) {
      return await invoke<SessionHit[]>('search_sessions', { query: q });
    }
    const res = await fetch(
      `${backendBaseUrl()}/api/sessions/search?q=${encodeURIComponent(q)}`,
    );
    if (!res.ok) return [];
    const data = await res.json();
    return data.hits || [];
  } catch {
    return [];
  }
}
```

- [ ] **Step 2: Type check + build**

```bash
cd /Users/yeheng/workspaces/Github/gasket/web && pnpm build   # vue-tsc -b && vite build
```

- [ ] **Step 3: Commit**

```bash
cd /Users/yeheng/workspaces/Github/gasket
git add web/src/lib/backend.ts
git commit -m "feat(web): searchSessions adapter over Tauri command / gateway REST"
```
