# conga-rag 个人 RAG 设计

> 状态:草案 · 日期 2026-08-29 · 基于 conga workspace 2.0.0

## 1. 目标

在 conga workspace 内新增 crate `conga-rag`:一个个人 RAG 系统。可配置的本地输入源,核心管线
「导入 → 清洗文本 → 分块 → embedding 计算 → 入库向量数据库」,无头命令行实现检索与问答。

构建在 conga 基座上的复用点:

- workspace 工程(fmt/clippy/CI、依赖版本统一)。
- 连接配置哲学:`CONGA_LLM_*` 环境变量风格、`~/.conga/` 数据目录、dotenvy 加载。
- `rag ask` 生成:直接调用 `conga::providers` 的 `StreamFn`(`OpenAiCompat`/`Anthropic`),
  空工具列表 + 两条消息,复用其流式与重试逻辑,不引入 agent loop。
- CLI 约定:对齐 `conga exec`(`--json` NDJSON 输出、退出码 0/1/2)。

## 2. 范围

| 在范围内 | 不在范围内(留缝不预建) |
|---|---|
| 本地目录输入源(md/txt/源码等纯文本) | PDF/EPUB/DOCX、网页 URL(`Source` trait 可扩展) |
| OpenAI 兼容 embedding API | 本地模型推理(fastembed 等) |
| sqlite-vec 向量检索(单文件库) | Qdrant/LanceDB 等外部服务或重依赖 |
| 纯文本清洗 + 结构感知分块 | BM25 混合检索、rerank |
| `ingest`/`search`/`ask`/`status` 四命令 | 文件 watcher 自动重嵌、Web UI |
| 文档级增量(hash/mtime 检测) | chunk 级 diff |

## 3. 已定决策

| 决策点 | 结论 | 依据 |
|---|---|---|
| 项目形态 | workspace 内单 crate(lib + bin) | 单消费者(CLI),拆 crate 是仪式感 |
| 输入源 | 首期仅本地目录文档 | 用户默认(两次多选未选) |
| embedding | OpenAI 兼容 API | 用户选择 |
| 检索出口 | search + ask(生成) | 用户选择 |
| 向量存储 | sqlite-vec(vec0 虚拟表,KNN 下推) | 用户选择(方案 B) |

## 4. 总体架构

```
conga-rag/  (crate,lib 名 conga_rag,bin 名 conga-rag)
├── src/lib.rs        管线库
│   ├── config.rs     TOML 配置 + CONGA_RAG_* env 覆盖
│   ├── source/       Source trait + DirSource(ignore 遍历 + globset 过滤 + 变更检测)
│   ├── clean.rs      纯函数:BOM/换行规范化、压缩空行(保留 markdown 标题结构)
│   ├── chunk.rs      纯函数:结构感知分块(段落/标题边界优先,目标窗 + overlap)
│   ├── embed.rs      EmbeddingsClient:批量 POST /embeddings、指数退避重试
│   ├── store.rs      SQLite(rusqlite bundled)+ sqlite-vec:documents/chunks/vec_chunks/meta
│   ├── search.rs     查询向量 → vec0 KNN → 装配 (source, path, score, content)
│   └── ask.rs        top-k 拼上下文 → conga StreamFn 流式生成 → 尾部来源引用
└── src/main.rs       clap CLI
```

数据流:

```
ingest:  DirSource 枚举文件
           → content_hash 未变则跳过;已删文件的 chunks/向量随事务清除
           → clean → chunk → 批量 embed → 单事务文档级 upsert(先删后插)
search:  query → embed(单条) → vec0 KNN(distance_metric=cosine) → 装配结果
ask:     search(top_k) → 上下文模板 → StreamFn 流式输出 → 引用来源列表
```

## 5. 配置设计

文件发现顺序:`./rag.toml`(当前目录)→ `~/.conga/rag.toml`;`CONGA_RAG_CONFIG` 可指定路径。
env 覆盖文件值。

```toml
[sources.notes]                    # 段名即 --source 过滤键
type = "dir"                       # 首期仅 "dir"
path = "~/notes"                   # 支持 ~ 展开
include = ["**/*.md", "**/*.txt"]  # globset
exclude = ["**/drafts/**"]

[embedding]
base_url = "https://api.zhipu.cn/v1"  # 缺省回落 CONGA_LLM_BASE_URL
api_key  = "..."                      # 缺省回落 CONGA_LLM_KEY
model    = "embedding-3"
batch    = 64                         # 每次请求的输入条数

[chunking]
target_chars  = 1200
overlap_chars = 200

[store]
path = "~/.conga/rag/index.db"

[ask]                                 # 生成复用 CONGA_LLM_* 全组(conga ProviderConfig)
top_k = 6
```

## 6. 存储设计(sqlite-vec)

连接:每命令打开一个连接;`rusqlite`(bundled feature,自带 SQLite)+ `sqlite-vec` 扩展
(C 源经 cc 静态编译进二进制,`LoadExtensionGuard` 加载,无运行时 dylib 依赖)。

```sql
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
-- 记录 schema_version、embedding_model、embedding_dim

CREATE TABLE documents(
  id INTEGER PRIMARY KEY,
  source TEXT NOT NULL,          -- 配置段名
  path TEXT NOT NULL,            -- 绝对路径
  mtime INTEGER NOT NULL,
  content_hash TEXT NOT NULL,    -- 清洗后内容 sha256
  chunk_count INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(source, path)
);

CREATE TABLE chunks(
  rowid INTEGER PRIMARY KEY,     -- 与 vec_chunks.rowid 一一对应
  doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  ordinal INTEGER NOT NULL,
  content TEXT NOT NULL
);

CREATE VIRTUAL TABLE vec_chunks USING vec0(
  embedding float[<dim>] distance_metric=cosine
);
```

- **dim 固化于建表语句**:首版 ingest 时,取首次 embedding 响应的实际维度建表并写入 meta。
  之后每次 ingest 校验响应维度;换 embedding 模型(名称或维度变化)→ 明确报错并提示
  `--rebuild`(删库重建),绝不静默混存两套向量空间。
- **KNN 查询**:
  ```sql
  SELECT v.rowid, v.distance
  FROM vec_chunks v
  WHERE v.embedding MATCH :query AND v.k = :k
    AND v.rowid IN (SELECT c.rowid FROM chunks c
                    JOIN documents d ON d.id = c.doc_id
                    WHERE d.source = :source)   -- 无 -s 过滤时省略
  ORDER BY v.distance;
  ```
  sqlite-vec 的 KNN 支持 `rowid IN` 预过滤;若所装版本行为不符,降级为超采(k×4)+ 后过滤。
- **删除**:文档重嵌/移除时,同事务内 `DELETE FROM vec_chunks WHERE rowid IN (...)` +
  `DELETE FROM chunks ...` + `DELETE FROM documents ...`。
- **备份 = 拷贝单个 .db 文件**。

## 7. 管线细节

### 7.1 DirSource

- `ignore` crate 遍历(自动尊重 .gitignore)+ globset include/exclude 二次过滤。
- 变更检测:以 `content_hash`(清洗后内容)为准——mtime 与上次一致则跳过读取(快路径);
  mtime 变了才读取清洗并算 hash,hash 相同则仅更新 mtime 记录、不重嵌。
- 磁盘上消失的文件 → 删除其 documents/chunks/vec_chunks(事务内)。

### 7.2 clean(纯函数)

去 BOM、统一 CRLF→LF、行尾空白、压缩 ≥3 空行为 2;markdown 不剥语法(标题/列表保留,
结构信息留给 chunk 用)。

### 7.3 chunk(纯函数)

- 输入:清洗后文本 + target_chars + overlap_chars。
- 边界优先级:一级/二级标题 > 空行(段落)> 硬切。
- 单段超窗:按句子/空格软切;代码块(``` 围栏)尽量不切断。
- 产出:`Vec<String>`,记录 ordinal。

### 7.4 EmbeddingsClient

- `POST {base_url}/embeddings`,body `{model, input: [texts...]}`,按 `batch` 分批。
- 重试:429/5xx 指数退避 + 抖动(参数沿用 conga retry 风格,默认 2 次);耗尽则中止,
  已完成批次的事务保留(文档级原子,无半截文档)。
- 校验:响应条数与维度;维度与 meta 不符 → 报错。

## 8. ask 生成

- `search(top_k)` 结果按分数排序拼上下文模板(编号 + 来源路径 + 内容)。
- 调 `conga::providers` 的 `StreamFn`(经 `ProviderConfig` 读 `CONGA_LLM_*`,
  支持 openai/anthropic 两种协议、代理与重试),`tools = &[]`,
  system prompt 限定「仅依据给定资料回答,标注引用编号」。
- 终端流式打印答案,尾部输出引用列表(编号 → 来源路径 + 分数)。
- `--json` 模式:逐 token/块 NDJSON 输出(对齐 conga exec 事件流风格),末尾一条
  引用汇总事件。

## 9. CLI 契约(clap;退出码 0 成功 / 1 运行错误 / 2 用法错误)

```
conga-rag ingest  [-s <source>] [--rebuild] [--json]
conga-rag search  <query> [-k N(默认5)] [-s <source>] [--json]
conga-rag ask     <question> [-k N(默认取 ask.top_k)] [--json]
conga-rag status
```

| 命令 | 文本输出 | --json 输出 |
|---|---|---|
| ingest | 统计行(scanned/added/updated/removed/skipped/failed) | 每源一条 NDJSON 统计对象 + 汇总 |
| search | 每命中:分数、相对路径、片段 | 每命中一条 NDJSON(query/score/source/path/content) |
| ask | 流式答案 + 引用列表 | 流式 NDJSON 块 + 引用汇总事件 |
| status | 各 source 文档/chunk 数、模型指纹 | 一个汇总对象 |

空索引 search → 明确提示「索引为空,先运行 conga-rag ingest」,退出码 1。

## 10. 错误处理与边界

- 单文件读取/解析失败:计数并继续,ingest 结尾汇总;全部源无成功文件才退出码 1。
- embedding/生成 API 失败:重试耗尽 → 报错退出 1,已完成文档的事务保留。
- 配置缺失(sources 为空 / embedding 配置不全):启动即报错,指出缺哪项。
- 幂等:连续两次 ingest,第二次 added=0 updated=0 removed=0(回归断言)。

## 11. 依赖(新增)

`rusqlite`(bundled)、`sqlite-vec`、`toml`、`globset`、`ignore`、`sha2`;
其余对齐 workspace 依赖版本(clap/reqwest/serde/anyhow/tracing/dirs/dotenvy 等)。

## 12. 测试策略

- **clean/chunk 纯函数单测**:BOM/CRLF、空行压缩、标题切分、单段超窗软切、overlap 正确性、
  代码块不切。
- **store 集成测试**(tempfile):vec0 KNN 正确性(已知向量最近邻)、文档级 upsert 幂等、
  删除清理(含虚拟表)、模型指纹变更拒绝、`--rebuild` 重建。
- **embed mock 测试**:axum 本地 mock server 验证分批、429 退避重试、维度校验失败。
- **ask 测试**:in-crate fake `StreamFn`(对齐 conga test_util 模式)验证上下文模板与
  引用装配。
- **端到端冒烟**:临时目录 + mock embedding server → ingest ×2(第二次零变更)→
  search 断言 top-1 命中目标文档。

## 13. 验收

1. `cargo test --all-features`、`cargo clippy --all-features -- -D warnings`、
   `cargo fmt --all -- --check` 全绿(含新 crate)。
2. 真实 API 下手动冒烟:`conga-rag ingest` 建库;`conga-rag search` 返回相关片段;
   `conga-rag ask` 生成带引用答案;重复 ingest 零变更。
3. `conga-rag status` 正确展示各源统计与模型指纹。

## 14. YAGNI 清单(明确不做)

PDF/Office/URL 输入源、文件 watcher、BM25 混合检索、rerank、多语言分块策略、
chunk 级增量 diff、向量缓存、Web UI、并发 embedding 请求。
