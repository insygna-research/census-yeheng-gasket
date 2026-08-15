# gasket 使用文档

> 本文教你如何把 gasket 跑起来:配置 LLM、运行后端网关与 CLI、启动 Web / 桌面前端、容器化部署。
> 想理解内部架构,请阅读 [架构设计](./architecture.md)。

gasket 由两部分组成:

- **后端**(`gasket/`,Rust 工作区):一个 WebSocket 网关服务器 `gasket-gateway`,外加一个终端 REPL `gasket`。
- **前端**(`web/`,Vue 3):Web 应用,同一份代码可打包成 Tauri 桌面应用。前端经 `ws://<host>:3000` 连接后端网关。

---

## 1. 环境要求

| 组件 | 用途 | 版本/说明 |
|---|---|---|
| **Rust** 工具链 | 构建后端 | 稳定版(stable);`cargo`、`rustc` |
| **Node.js** + **pnpm** | 构建/开发前端 | Node ≥ 18;包管理用 **pnpm**(仓库已标准化) |
| **LLM API Key** | 接入大模型 | 任一 OpenAI 兼容服务(DeepSeek/智谱/xAI/Groq/Ollama/vLLM 等)或 Anthropic |
| **Tauri 2 系统依赖**(仅桌面端) | 打包桌面应用 | macOS:Xcode CLT;Windows:MSVC + WebView2;Linux:webkit2gtk 等 |

> 后端 release profile 已做体积优化(`opt-level="z"`、`lto="fat"`、`strip`),最终二进制较小。

---

## 2. 快速开始(5 分钟)

```bash
# 1) 克隆
git clone https://github.com/YeHeng/gasket.git
cd gasket

# 2) 配置后端 LLM(必填三项)
cp gasket/.env.example gasket/.env
#   编辑 gasket/.env:
#     GASKET_LLM_BASE_URL=https://api.deepseek.com/v1
#     GASKET_LLM_KEY=你的-key
#     GASKET_LLM_MODEL=deepseek-chat
#     GASKET_LLM_API=openai        # 或 anthropic

# 3) 启动后端网关(监听 0.0.0.0:3000,并托管前端)
cd gasket && cargo run --release --bin gasket-gateway

# 4) 另开终端,启动 Web 前端(浏览器开发模式,端口 1420)
cd web && pnpm install && pnpm dev
#   打开 http://localhost:1420,开始对话
```

> 也可以跳过前端,直接用终端:`cd gasket && cargo run --release --bin gasket`(见 §4.2)。

---

## 3. 后端配置(`.env`)

后端所有配置走**环境变量 + dotenvy**(`gasket/.env`)。模板见 `gasket/.env.example`(已覆盖常用变量,完整清单见 §10)。

### 3.1 必填:LLM 连接

| 变量 | 说明 | 示例 |
|---|---|---|
| `GASKET_LLM_BASE_URL` | provider 基础 URL | `https://api.deepseek.com/v1` |
| `GASKET_LLM_KEY` | API key | `sk-...` |
| `GASKET_LLM_MODEL` | 模型 id | `deepseek-chat` |
| `GASKET_LLM_API` | 协议族:`openai`(默认)或 `anthropic` | `openai` |

### 3.2 Provider 选择

- **OpenAI 兼容(`openai`,默认)**:DeepSeek、智谱 GLM、xAI、Groq、Ollama、vLLM 等任填 base_url + key + model 即可。
- **Anthropic(`anthropic`)**:设 `GASKET_LLM_API=anthropic`,base_url 指向 `https://api.anthropic.com/v1`,用 Claude 模型。

### 3.3 可选:代理

| 变量 | 说明 |
|---|---|
| `GASKET_LLM_PROXY` | http 与 https 通吃的代理(fallback) |
| `GASKET_LLM_HTTP_PROXY` | 仅 http(覆盖上面的 http 部分) |
| `GASKET_LLM_HTTPS_PROXY` | 仅 https(覆盖上面的 https 部分) |

代理优先级:按 scheme 的专用代理(`GASKET_LLM_HTTP_PROXY`/`GASKET_LLM_HTTPS_PROXY`)最高;`GASKET_LLM_PROXY` 填补缺失的那个 scheme。不读取标准的 `HTTP_PROXY`/`HTTPS_PROXY` 环境变量。

**工具代理(fetch / web_search)**:设置 `GASKET_TOOL_PROXY` 可让 `fetch` 与 `web_search` 工具的出站流量走代理,支持 `http` / `https` / `socks5` / `socks5h`(带认证的代理把 `user:pass` 写进 URL 即可):

| 变量 | 说明 | 示例 |
|---|---|---|
| `GASKET_TOOL_PROXY` | 工具出站代理 | `socks5://127.0.0.1:1080` |

桌面版在顶栏 Globe 按钮中配置代理,优先级高于该环境变量;保存后下一次工具调用即生效,无需重启。该代理不影响 LLM API 请求(那部分继续用上面的 `GASKET_LLM_PROXY` 系列)。
点击 Disable 时若设置了 GASKET_TOOL_PROXY 则回退到该环境变量，而非直连。

注意 fail-open 语义:`GASKET_TOOL_PROXY` 里的无效 URL(拼错、不支持 scheme)不会报错中断,只会打一条 warn 日志然后**静默回退直连**。桌面版 UI 保存时会做完整校验,坏值进不了配置;环境变量没有这道关卡,若你在意"绝不经由直连暴露流量"(例如绕封锁场景),请自行确认该变量值有效。

**fetch 内网防护(SSRF guard)**:`fetch` 工具默认拒绝访问非公网地址——IP 直连(127.x、10.x、172.16-31.x、192.168.x、169.254.x、`::1` 等)与解析到这些网段的主机名一律拦截,防止模型借工具探测内网或云元数据端点(如 `http://169.254.169.254/`)。例外:

| 变量 | 说明 | 示例 |
|---|---|---|
| `GASKET_FETCH_ALLOW_PRIVATE_NET` | 置 `1`/`true` 放行内网目标(信任的自托管局域网) | `1` |

配置了 `GASKET_TOOL_PROXY`(或桌面版代理)时,该防护整体跳过——出站走向由代理决定。

---

## 4. 运行后端

### 4.1 网关服务器 `gasket-gateway`(给 Web/桌面端用)

```bash
cd gasket
cargo run --release --bin gasket-gateway
```

- 默认监听 `0.0.0.0:3000`(`GASKET_GATEWAY_PORT` 可改)。
- 自动托管 `web/dist` 静态资源(`GASKET_GATEWAY_STATIC_DIR` 可改,默认 `../web/dist`)——**先 `pnpm build` 出 dist,网关就能直接serve 整个 Web 应用**(无需单独跑前端服务器)。
- 暴露:WebSocket `/ws`、REST `/api/commands`、`/api/sessions`、`/api/sessions/{key}/context`、`/api/sessions/{key}/context/compact`、`/api/sessions/{key}/messages`(后端真相端点:对磁盘 `events.jsonl` 跑 `derive_messages`,未知 key→404、损坏日志→500)。
- **会话存储**:每个会话是 `~/.gasket/sessions/<id>/events.jsonl` 的一份**崩溃安全事件日志**——一轮里每个已发生的事实(助手消息、工具结果)在它发生时就落盘,而非等到整轮成功才追加;崩溃 / 失败 / 中断的轮次仍保有其已经发生的全部副作用。旧 `messages.jsonl` 会话首次打开时自动迁移并删除旧文件(不可逆)。详见 [架构 §5.5](./architecture.md) 与 [ADR 0001](./adr/0001-event-sourced-session-log.md)。

### 4.2 终端 REPL `gasket`(纯命令行)

```bash
cd gasket
cargo run --release --bin gasket
# 带选项:
cargo run --release --bin gasket -- --mode=full-auto --resume=last
```

- 启动后进入交互式 REPL,每行输入触发一轮对话。
- **启动参数**:`--mode=<suggest|auto-edit|full-auto>`(默认 `auto-edit`)、`--resume=<id|last>`(恢复会话)。
- **斜杠命令**(输入 `/` 开头):

| 命令 | 作用 |
|---|---|
| `/help` | 列出命令 |
| `/mode <suggest\|auto-edit\|full-auto>` | 切换权限模式 |
| `/resume [id\|last]` | 恢复会话(默认 last) |
| `/clear` | 开新会话 |
| `/sessions` | 列出会话 |
| `/reload-tools` | 重新加载外部工具 |
| `/exit` | 退出 |

- **Ctrl-C**:在流式输出中触发**协作式中止**(在下一个安全点退出,返回已生成的部分);在输入行是 reedline 按键事件。
- **工具审批**:取决于模式与工具风险,可能弹出 `[approve <tool>? y/N]`,输入 `y` 放行。

### 4.3 编译内置扩展(可选)

CLI 默认不带进程内扩展。需要 `hello`/`todo`/`search`/`permission_gate` 时,启用 feature:

```bash
cargo run --release --bin gasket --features ext
```

### 4.4 测试与质量门禁

```bash
cd gasket
cargo test --all-features
cargo fmt --all -- --check
cargo clippy --all-features -- -D warnings
```

---

## 5. Web 端(浏览器)

前端位于 `web/`,pnpm 管理。

```bash
cd web
pnpm install

# 开发模式(Vite dev server,默认端口 1420,带 HMR)
pnpm dev

# 生产构建(先 vue-tsc 类型检查,再 vite build → dist/)
pnpm build

# 预览生产构建
pnpm preview
```

### 5.1 指向后端网关

前端连接地址由 env 控制。编辑 `web/.env`(模板见 `web/.env.example`):

| 变量 | 默认 | 说明 |
|---|---|---|
| `VITE_WS_URL` | `ws://localhost:3000` | WebSocket 地址(流式对话) |
| `VITE_API_URL` | `http://localhost:3000` | REST 地址(上下文元数据) |

> 联调时:先起后端 `cargo run --bin gasket-gateway`,前端 `VITE_WS_URL` 指向它;二者默认端口已对齐(3000)。

### 5.2 两种部署形态

- **前后端同源(推荐)**:`pnpm build` 出 `dist/`,让 `gasket-gateway` 托管它(见 §4.1)。浏览器访问 `http://<gateway>:3000/` 即用,无跨域、无单独前端服务器。
- **前后端分离**:单独跑 `pnpm dev` / 部署 `dist/` 到任意静态服务器,`VITE_WS_URL` 指向后端网关(网关已放开 CORS)。

---

## 6. Tauri 桌面端

桌面端用同一份 `web/src`,Tauri 把 Vite 产物装进原生窗口。

```bash
cd web
pnpm install

# 开发模式(自动调 pnpm dev,连 localhost:1420)
pnpm tauri:dev      # = tauri dev

# 构建分发包(自动调 pnpm build,产 .dmg/.msi/.exe)
pnpm tauri:build    # = tauri build
```

- 产物:`web/src-tauri/` 配置中 `productName=Gasket`、`identifier=com.gasket.desktop`、`bundle.targets=all`。macOS 出 `.dmg`,Windows 出 `.msi`/`.exe`。
- 配置见 `web/src-tauri/tauri.conf.json`:`frontendDist=../dist`、`devUrl=http://localhost:1420`。

> **桌面端是自包含的**:Tauri 桌面端内置进程内 Host(`src-tauri/src/chat.rs`),通过 IPC 直接做推理,不需要独立 gateway。但仍需 LLM API key 和 `~/.gasket` 配置(与 gateway 共用同一套)。浏览器版则需要独立部署的 gateway。

---

## 7. Docker 部署

仓库根 `Dockerfile` 是可用的多阶段构建:构建阶段编译 Rust workspace 全部 5 个 crate 并 `pnpm build` 产出 `web/dist`,运行阶段 `GASKET_GATEWAY_STATIC_DIR=/app/web/dist`、`EXPOSE 3000`、`ENTRYPOINT ["gasket-gateway"]`。

```bash
docker build -t gasket .
docker run -d -p 3000:3000 \
  -e GASKET_LLM_BASE_URL=https://api.deepseek.com/v1 \
  -e GASKET_LLM_KEY=sk-... \
  -e GASKET_LLM_MODEL=deepseek-chat \
  -e GASKET_LLM_API=openai \
  --name gasket gasket:latest
# 访问 http://localhost:3000/
```

运行时通过 `-e` 或 `--env-file` 注入 `GASKET_LLM_*` 等环境变量(完整清单见 §10)。

---

## 8. 上下文压缩配置

压缩在喂给 LLM 前缩小工作内存(只缩内存、不改盘、无 LLM 摘要)。详见 [架构设计 §9](./architecture.md)。相关环境变量:

| 变量 | 默认 | 说明 |
|---|---|---|
| `GASKET_CONTEXT_WINDOW` | `128000` | 模型上下文窗口(token) |
| `GASKET_COMPACT_THRESHOLD_PCT` | `80` | 占用超过窗口的该百分比时触发压缩 |
| `GASKET_COMPACT_TARGET_PCT` | `50` | 压缩后目标占窗口的百分比(带滞后,防抖) |
| `GASKET_COMPACT_MAX_MESSAGES` | `80` | 无 provider usage 数据时,按消息条数兜底的阈值;`0` 表示不压缩 |

Web 端可在头部点 **Compress** 按钮手动触发(调用 `POST /api/sessions/{key}/context/compact`,见 §4.1)。

---

## 9. 工具与权限

### 9.1 内置工具

`read` / `write` / `edit` / `bash` / `grep` / `list` / `fetch`(详见 [架构设计 §5.2](./architecture.md))。每个工具自带风险等级:`read`/`grep`/`list`/`fetch` 为低风险,`write`/`edit` 为中风险,`bash` 为高风险。默认 `auto-edit` 模式下低/中风险自动放行,仅高风险(`bash`)请求审批。`fetch` 工具抓取 URL 并把 HTML 转成可读文本(markdown 风格),支持 http/https。`bash`/`fetch` 超过 200KB 的输出会完整落盘到 `~/.gasket/tool_state/<会话>/<工具>/spill/`,上下文中只保留头部预览与文件路径(完整输出保留在磁盘上该路径,用户可自行查看或经 shell 取回)。

### 9.2 外部工具(白名单)

通过环境变量 `GASKET_EXTERNAL_TOOLS`(逗号分隔的命令白名单)接入外部命令工具,启动时加载;CLI 里可用 `/reload-tools` 热重载。

```bash
# 例:允许把 rg、jq 作为工具暴露给 agent
GASKET_EXTERNAL_TOOLS=rg,jq
```

### 9.3 MCP 工具服务器

[Model Context Protocol](https://modelcontextprotocol.io)(MCP)是一个开放协议,生态里有大量现成工具服务器(GitHub、文件系统、数据库、浏览器、Slack…)。gasket 作为 MCP 客户端,把这些 server 暴露的 tools 接进来,与内置工具同列供 agent 调用。

**配置文件**:`~/.gasket/mcp.json`(或用 `$GASKET_MCP_CONFIG` 指定路径)。文件不存在 = 不加载任何 MCP 工具(静默,不报错)。格式与 Claude Desktop、Cline 等主流客户端一致,可直接复用现有配置。支持两种传输方式,可在同一配置文件中混用:

#### stdio 传输(本地子进程)

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_xxx" }
    },
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/user/docs"]
    }
  }
}
```

每个 server 是 `mcpServers` map 里的一项,key 即 server 名(用于工具名前缀),`command` + `args` 指定如何启动子进程,`env` 里的环境变量会**追加**到子进程(不替换父进程环境)。

#### Streamable HTTP 传输(远程服务器)

```json
{
  "mcpServers": {
    "remote-sentry": {
      "url": "https://mcp.sentry.dev/mcp",
      "headers": { "Authorization": "Bearer your-token-here" }
    }
  }
}
```

`url` 指向远程 MCP server 的 HTTP 端点,`headers` 里的键值对会作为 HTTP 请求头随每个 JSON-RPC POST 发送(常用于 `Authorization: Bearer ...`)。stdio(`command`)和 HTTP(`url`)在同一 server 条目中互斥,但不同 server 可以混用两种传输。

**工具命名**:MCP 工具名加前缀 `mcp__{server名}__{工具名}`(如 `mcp__github__create_issue`),避免与内置工具或跨 server 重名。所有 MCP 工具的**风险等级统一为 High**——在非 full-auto 模式下会请求审批。

**工作原理**:
- **stdio**:gasket 为每个配置项 spawn 子进程,运行 MCP `initialize` 握手 → `tools/list` 发现工具 → 每个 MCP tool 包装成一个 `ToolDefinition`。Agent 调用工具时,gasket 发送 `tools/call`,server 返回结果(text/image 内容)。子进程在工具被 drop 时自动终止。
- **Streamable HTTP**:gasket 向 server URL POST JSON-RPC 请求,响应可能是单个 JSON 或 SSE 流(`text/event-stream`)。无状态会话——每个请求独立 POST。支持 `GASKET_LLM_PROXY` / `HTTPS_PROXY` 代理。

**支持范围**(当前版本):

- 传输:**stdio**(子进程)+ **Streamable HTTP**(远程服务器)。
- 协议:**legacy era**(`initialize` 握手,协议版本 `2025-06-18`)。覆盖现存几乎所有 MCP server。
- 原语:仅 **tools**。不接 resources / prompts / sampling / elicitation。
- 内容:text + image。未知类型降级为文本描述(不丢信息)。
- 超时:单次 `tools/call` 超时由 `GASKET_MCP_CALL_TIMEOUT_S` 控制(默认 60 秒)。

> **安全提示**:`mcp.json` 可能含 API key 等密文。确保该文件不被提交到版本控制(`~/.gasket/` 默认在 home 目录外,不受仓库 gitignore 影响)。

### 9.4 搜索扩展

启用 `ext` feature 后,`search` 工具支持多家 provider,由 `GASKET_SEARCH_PROVIDER` 选择,并填对应 key:

| 变量 |
|---|
| `GASKET_SEARCH_PROVIDER` |
| `GASKET_BRAVE_API_KEY` / `GASKET_TAVILY_API_KEY` / `GASKET_SERPER_API_KEY` / `GASKET_SERPAPI_API_KEY` / `GASKET_EXA_API_KEY` / `GASKET_FIRECRAWL_API_KEY` |

### 9.5 权限模式

| 模式 | 行为 |
|---|---|
| `suggest` | 只读:低风险放行,中/高风险直接拒绝 |
| `auto-edit` | 低/中风险自动放行,高风险请求审批(**CLI 默认**,gateway 默认 `auto-edit`) |
| `full-auto` | 全部自动放行(慎用) |

入口设定:CLI 用 `--mode=` 或 `/mode`;gateway 用 `GASKET_GATEWAY_MODE`;审批超时用 `GASKET_APPROVAL_TIMEOUT_S`(默认 300 秒)。

---

## 10. 环境变量完整参考

> `gasket/.env.example` 模板已覆盖常用变量,本表为完整参考。

### LLM 连接(必填三项)

| 变量 | 默认 | 说明 |
|---|---|---|
| `GASKET_LLM_BASE_URL` | — | provider 基础 URL(必填) |
| `GASKET_LLM_KEY` | — | API key(必填) |
| `GASKET_LLM_MODEL` | — | 模型 id(必填) |
| `GASKET_LLM_API` | `openai` | `openai` 或 `anthropic` |
| `GASKET_LLM_PROXY` / `GASKET_LLM_HTTP_PROXY` / `GASKET_LLM_HTTPS_PROXY` | — | 代理(见 §3.3) |

### 推理循环旋钮(均可选)

| 变量 | 默认 | 说明 |
|---|---|---|
| `GASKET_MAX_TURNS` | `50` | 外层循环最大轮数 |
| `GASKET_MAX_TOOL_CALLS` | `20` | 单轮内工具调用上限 |
| `GASKET_MAX_TOKENS` | `4096` | 模型输出 token 上限 |
| `GASKET_THINKING` | `off` | `off`/`low`/`medium`/`high`(模型不支持时无效) |
| `GASKET_RETRY_MAX` | `2` | LLM 调用最大重试次数(仅流前失败) |
| `GASKET_RETRY_INITIAL_MS` | `500` | 首次重试退避(ms) |
| `GASKET_RETRY_MAX_MS` | `8000` | 退避上限(ms) |

### 网关服务器

| 变量 | 默认 | 说明 |
|---|---|---|
| `GASKET_GATEWAY_PORT` | `3000` | 监听端口 |
| `GASKET_GATEWAY_STATIC_DIR` | `../web/dist` | 前端静态资源目录 |
| `GASKET_GATEWAY_MODE` | `auto-edit` | 审批模式 `suggest`/`auto-edit`/`full-auto` |
| `GASKET_APPROVAL_TIMEOUT_S` | `300` | 审批等待超时(秒) |

### 上下文压缩

| 变量 | 默认 | 说明 |
|---|---|---|
| `GASKET_CONTEXT_WINDOW` | `128000` | 模型上下文窗口 |
| `GASKET_COMPACT_THRESHOLD_PCT` | `80` | 触发压缩阈值(%) |
| `GASKET_COMPACT_TARGET_PCT` | `50` | 压缩后目标(%) |
| `GASKET_COMPACT_MAX_MESSAGES` | `80` | 条数兜底(`0`=不压缩) |

### 工具 / 搜索 / MCP

| 变量 | 说明 |
|---|---|
| `GASKET_EXTERNAL_TOOLS` | 外部命令工具白名单(逗号分隔,见 §9.2) |
| `GASKET_MCP_CONFIG` | MCP 配置文件路径(默认 `~/.gasket/mcp.json`,见 §9.3) |
| `GASKET_SEARCH_PROVIDER` | 搜索 provider 选择(需 `ext` feature) |
| `GASKET_BRAVE_API_KEY` / `GASKET_TAVILY_API_KEY` / `GASKET_SERPER_API_KEY` / `GASKET_SERPAPI_API_KEY` / `GASKET_EXA_API_KEY` / `GASKET_FIRECRAWL_API_KEY` | 各搜索商 key |

### 前端(`web/.env`)

| 变量 | 默认 | 说明 |
|---|---|---|
| `VITE_WS_URL` | `ws://localhost:3000` | WebSocket 地址 |
| `VITE_API_URL` | `http://localhost:3000` | REST 地址 |

---

## 11. 故障排查 / FAQ

| 现象 | 排查 |
|---|---|
| CLI 启动报 `config error` 并退出 | 三项必填 env 缺失。确认 `gasket/.env` 有 `GASKET_LLM_BASE_URL`/`GASKET_LLM_KEY`/`GASKET_LLM_MODEL`,或在 shell 里 `export`。 |
| Web 端连不上、一直离线 | `VITE_WS_URL` 指向错或后端未起。确认 `gasket-gateway` 在跑(默认 3000),且 `VITE_WS_URL=ws://localhost:3000`。重连 5 次后会显示手动 Reconnect 按钮。 |
| 端口 3000 被占用 | 用 `GASKET_GATEWAY_PORT=<其它端口>` 改网关端口,并把前端 `VITE_WS_URL`/`VITE_API_URL` 同步改掉。 |
| 报 `orphan tool_call` / 工具结果错乱 | 通常与压缩有关;确认没有手动设异常小的 `GASKET_COMPACT_MAX_MESSAGES`。正常情况下原子组会保护 tool_call↔result。 |
| 模型不支持 thinking | `GASKET_THINKING` 设了 `low/medium/high` 但模型不支持时自动无效化;`ModelSpec.supports_thinking` 控制是否发送 thinking 参数。 |
| 桌面端打不开/不响应 | 确认 LLM API key 已配置(环境变量或 `gasket/.env`);桌面端通过进程内 Host 做 IPC 推理,不需要独立 gateway。 |
| 想用 Claude(Anthropic) | `GASKET_LLM_API=anthropic`,`GASKET_LLM_BASE_URL=https://api.anthropic.com/v1`,`GASKET_LLM_MODEL=claude-...`。 |
| 想接本地 Ollama/vLLM | `GASKET_LLM_API=openai`(默认),`GASKET_LLM_BASE_URL=http://localhost:11434/v1`(Ollama 示例),key 随意填。 |
| MCP server 启动失败 / 工具不出现 | 确认 `command` 在 `PATH` 里(如 `npx`);检查 `~/.gasket/mcp.json` JSON 合法;server 自身的 `env`(API key)正确。单个 server 失败不影响其他 server 和内置工具。 |

---

## 12. 速查

```bash
# 后端网关(托管前端,一键起 Web 服务)
cd gasket && cargo run --release --bin gasket-gateway

# 终端 REPL
cd gasket && cargo run --release --bin gasket

# 前端开发(浏览器,1420)
cd web && pnpm install && pnpm dev

# 前端生产构建(交给网关托管)
cd web && pnpm build

# 桌面端
cd web && pnpm tauri:dev      # 开发
cd web && pnpm tauri:build    # 打包
```

> 进一步了解分层、数据流、工具系统、压缩算法与设计取舍,见 [架构设计](./architecture.md)。
