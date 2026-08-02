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

后端所有配置走**环境变量 + dotenvy**(`gasket/.env`)。模板见 `gasket/.env.example`(注:模板不完整,完整变量见 §9)。

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

代理优先级:按 scheme 的专用代理(`HTTP_PROXY`/`HTTPS_PROXY`)最高;`GASKET_LLM_PROXY` 填补缺失的那个 scheme。

---

## 4. 运行后端

### 4.1 网关服务器 `gasket-gateway`(给 Web/桌面端用)

```bash
cd gasket
cargo run --release --bin gasket-gateway
```

- 默认监听 `0.0.0.0:3000`(`GASKET_GATEWAY_PORT` 可改)。
- 自动托管 `web/dist` 静态资源(`GASKET_GATEWAY_STATIC_DIR` 可改,默认 `../web/dist`)——**先 `pnpm build` 出 dist,网关就能直接serve 整个 Web 应用**(无需单独跑前端服务器)。
- 暴露:WebSocket `/ws`、REST `/api/commands`、`/api/sessions/{key}/context`、`/api/sessions/{key}/context/compact`。

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

> **桌面端仍需后端**:Tauri 壳不做本地推理、不内置 LLM。它和浏览器版一样,经 `VITE_WS_URL`(打包时写死或运行时配置)连接一个 gasket-gateway。换句话说,桌面应用 = 一个原生窗口壳 + 远端(或本机)的 gateway。

---

## 7. Docker 部署

> ⚠️ **仓库根 `Dockerfile` 当前已过时,无法直接使用。** 它引用的是旧版目录结构(`gasket/types/`、`gasket/storage/`、`gasket/engine/` 等已不存在的路径),`EXPOSE 18790` 也对不上网关默认端口 **3000**,且默认 `ENTRYPOINT ["gasket"]` 跑的是 CLI 而非网关。使用容器部署前需要更新它。

**当前可行的容器化思路**(待补一个修复版 Dockerfile):

1. 基础镜像构建 Rust workspace,产出 `/usr/local/bin/gasket-gateway`。
2. 把 `web/dist`(先在构建阶段 `pnpm build` 得到)放进镜像,gateway 用 `GASKET_GATEWAY_STATIC_DIR` 指向它。
3. `EXPOSE 3000`,`ENTRYPOINT ["gasket-gateway"]`。
4. 运行时注入 `GASKET_LLM_*` 等环境变量(`-e` 或 `--env-file`)。

示例运行(假设已有修复版镜像):

```bash
docker run -d -p 3000:3000 \
  -e GASKET_LLM_BASE_URL=https://api.deepseek.com/v1 \
  -e GASKET_LLM_KEY=sk-... \
  -e GASKET_LLM_MODEL=deepseek-chat \
  -e GASKET_LLM_API=openai \
  --name gasket gasket:latest
# 访问 http://localhost:3000/
```

---

## 8. 上下文压缩配置

压缩在喂给 LLM 前缩小工作内存(只缩内存、不改盘、无 LLM 摘要)。详见 [架构设计 §9](./architecture.md)。相关环境变量:

| 变量 | 默认 | 说明 |
|---|---|---|
| `GASKET_CONTEXT_WINDOW` | `128000` | 模型上下文窗口(token) |
| `GASKET_COMPACT_THRESHOLD_PCT` | `80` | 占用超过窗口的该百分比时触发压缩 |
| `GASKET_COMPACT_TARGET_PCT` | `50` | 压缩后目标占窗口的百分比(带滞后,防抖) |
| `GASKET_COMPACT_MAX_MESSAGES` | `80` | 无 provider usage 数据时,按消息条数兜底的阈值;`0` 表示不压缩 |

Web 端可在头部点 **Compress** 按钮手动触发(调用 `POST /api/sessions/{key}/compact`)。

---

## 9. 工具与权限

### 9.1 内置工具

`read` / `write` / `edit` / `bash` / `grep` / `list`(详见 [架构设计 §5.2](./architecture.md))。每个工具自带风险等级,低风险(`read`/`grep`/`list`)通常自动放行,高风险(`write`/`edit`/`bash`)在非 full-auto 模式下会请求审批。

### 9.2 外部工具(白名单)

通过环境变量 `GASKET_EXTERNAL_TOOLS`(逗号分隔的命令白名单)接入外部命令工具,启动时加载;CLI 里可用 `/reload-tools` 热重载。

```bash
# 例:允许把 rg、jq 作为工具暴露给 agent
GASKET_EXTERNAL_TOOLS=rg,jq
```

### 9.3 MCP 工具服务器

[Model Context Protocol](https://modelcontextprotocol.io)(MCP)是一个开放协议,生态里有大量现成工具服务器(GitHub、文件系统、数据库、浏览器、Slack…)。gasket 作为 MCP 客户端,把这些 server 暴露的 tools 接进来,与内置工具同列供 agent 调用。

**配置文件**:`~/.gasket/mcp.json`(或用 `$GASKET_MCP_CONFIG` 指定路径)。文件不存在 = 不加载任何 MCP 工具(静默,不报错)。格式与 Claude Desktop、Cline 等主流客户端一致,可直接复用现有配置:

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

**工具命名**:MCP 工具名加前缀 `mcp__{server名}__{工具名}`(如 `mcp__github__create_issue`),避免与内置工具或跨 server 重名。所有 MCP 工具的**风险等级统一为 High**——在非 full-auto 模式下会请求审批。

**工作原理**:启动时,gasket 为每个配置项 spawn 子进程,运行 MCP `initialize` 握手 → `tools/list` 发现工具 → 每个 MCP tool 包装成一个 `ToolDefinition`。Agent 调用工具时,gasket 发送 `tools/call`,server 返回结果(text/image 内容)。子进程在工具被 drop 时自动终止。

**支持范围**(当前版本):

- 传输:**stdio**(子进程)。暂不支持 Streamable HTTP。
- 协议:**legacy era**(`initialize` 握手,协议版本 `2025-06-18`)。覆盖现存几乎所有 MCP server。
- 原语:仅 **tools**。不接 resources / prompts / sampling / elicitation。
- 内容:text + image。未知类型降级为文本描述(不丢信息)。

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
| `suggest` | 谨慎,广泛请求审批 |
| `auto-edit` | 低风险自动放行,其余请求审批(**CLI 默认**,gateway 默认 `auto-edit`) |
| `full-auto` | 全部自动放行(慎用) |

入口设定:CLI 用 `--mode=` 或 `/mode`;gateway 用 `GASKET_GATEWAY_MODE`;审批超时用 `GASKET_APPROVAL_TIMEOUT_S`(默认 300 秒)。

---

## 10. 环境变量完整参考

> `gasket/.env.example` 模板并不完整,以本表为准。

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
| 桌面端打不开/不响应 | 桌面端仍依赖后端网关,确认 gateway 可达、`VITE_WS_URL` 正确;Tauri 壳本身不做推理。 |
| 想用 Claude(Anthropic) | `GASKET_LLM_API=anthropic`,`GASKET_LLM_BASE_URL=https://api.anthropic.com/v1`,`GASKET_LLM_MODEL=claude-...`。 |
| 想接本地 Ollama/vLLM | `GASKET_LLM_API=openai`(默认),`GASKET_LLM_BASE_URL=http://localhost:11434/v1`(Ollama 示例),key 随意填。 |
| Docker 构建失败 | 根 `Dockerfile` 已过时(见 §7),需按当前 5-crate 结构重写。 |
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
