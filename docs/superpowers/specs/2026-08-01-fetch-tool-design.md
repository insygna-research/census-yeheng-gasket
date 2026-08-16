# URL 抓取工具(fetch)设计

> 状态:草案 · 日期 2026-08-01 · 阶段 2

## 1. 目标

新增内置工具 `fetch`:HTTP GET 一个 URL,把 HTML 转 markdown 喂给 agent。agent 现在能 `web_search`(ext)搜到 URL,或 `read` 读本地文件,但不能读网页正文。

## 2. 范围

| 在范围内 | 不在范围内 |
|---|---|
| HTTP GET → HTML → markdown 正文 | JS 渲染(无 headless browser) |
| 截断到 MAX_BYTES | 分页/无限滚动 |
| 仅 http/https scheme | POST/PUT 等方法 |
| 超时控制 | 认证/cookie |

## 3. 设计

```
工具: fetch
参数: { url: string (required) }
风险: Low (只读)
返回: ContentBlock::text(markdown 正文),截断到 MAX_OUTPUT_BYTES (200KB)
```

### 3.1 实现(`core/tools/fetch.rs`)

1. 验证 url scheme 是 http/https(拒绝 file://、ftp:// 等)。
2. `reqwest::get` 带 30s 超时 + UA header。
3. 只处理 `text/html` content-type;其他类型返回前 200KB 原始文本。
4. HTML → markdown:用 `dom_query::Document` 提取正文。
   - 移除 `<script>`/`<style>`/`<nav>`/`<footer>`/`<header>`。
   - 取 `<article>` 或 `<main>` 的文本;没有则取 `<body>`。
   - 基本格式保留:标题(`<h1>`→`#`)、链接(`<a>`→`[text](url)`)、列表(`<li>`→`-`)、段落(`<p>`→空行分隔)。
5. 截断到 `MAX_OUTPUT_BYTES`,末尾追加 `...(truncated)`。

### 3.2 依赖

`dom_query = "0.28"` 加入 `gasket-core/Cargo.toml`(已在 ext crate 验证过,纯 Rust)。

## 4. 测试

- **纯函数单测**:`html_to_markdown` 对简单 HTML 的转换(`<h1>`/`<p>`/`<a>`/`<ul>`)。
- **mock HTTP 集成测试**:用 `tokio::test` + 本地 TCP listener 返回固定 HTML,验证 fetch 工具端到端。
- **scheme 验证**:拒绝 `file://`。
- **截断**:大 HTML 截断到 MAX_OUTPUT_BYTES。

## 5. 验收

1. `fetch("https://example.com")` 返回 markdown 正文。
2. `cargo check + test` 全绿。
3. `built_in_tools()` 包含 fetch。
