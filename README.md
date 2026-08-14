# gasket

> A lightweight, self-hostable personal AI assistant framework — written in Rust.

gasket turns "an LLM agent that can call tools, stream output, manage sessions, and gate permissions" into a layered, reusable Rust workspace, with a Vue 3 web/desktop frontend.

## Features

- **Stateless agent loop** — the core reasoning loop is a pure function; all state lives in the host layer. Inject any LLM via the `StreamFn` trait.
- **8 built-in tools** — `read` / `write` / `edit` / `bash` / `grep` / `list` / `fetch` / `spawn_subagents`, each with a risk level (`Low` / `Medium` / `High`).
- **Hook chain & permissions** — `before_tool_call` (async, can block/modify) + `after_tool_call` (sync, can rewrite results). Three permission modes: `suggest` / `auto-edit` / `full-auto`.
- **Context compaction** — token-aware, turn-boundary-safe (never splits a `tool_call` from its `tool_result`), with provider-reported usage. Memory-only — the on-disk JSONL transcript is always append-only and complete.
- **MCP client** — connect [Model Context Protocol](https://modelcontextprotocol.io) tool servers (stdio + Streamable HTTP). Reuse existing Claude-Desktop-style `mcp.json` configs.
- **Subagent orchestration** — `spawn_subagents` tool fans out parallel sub-agent loops with real-time event streaming to the frontend.
- **Two frontends, one host** — a terminal REPL (`gasket` CLI) and a WebSocket gateway (`gasket-gateway`) both drive the same `Host::run_turn`. The Vue 3 frontend runs as a browser app or a Tauri desktop app from one codebase.
- **Crash-safe storage** — JSONL sessions with torn-tail self-healing (truncated last line on crash is auto-discarded; mid-file corruption reports with line numbers).

## Quick start (5 minutes)

```bash
# 1) Clone
git clone https://github.com/YeHeng/gasket.git
cd gasket

# 2) Configure the LLM (three required vars)
cp gasket/.env.example gasket/.env
#   Edit gasket/.env:
#     GASKET_LLM_BASE_URL=https://api.deepseek.com/v1
#     GASKET_LLM_KEY=your-key
#     GASKET_LLM_MODEL=deepseek-chat
#     GASKET_LLM_API=openai        # or anthropic

# 3) Start the backend gateway (serves the frontend too, once built)
cd gasket && cargo run --release --bin gasket-gateway

# 4) In another terminal, start the web frontend (dev mode, port 1420)
cd web && pnpm install && pnpm dev
#   Open http://localhost:1420
```

Prefer the terminal? Skip the frontend:

```bash
cd gasket && cargo run --release --bin gasket
```

## Architecture

gasket is a Cargo workspace with 5 crates, in a strict `core → host → frontends` layering:

| Crate | Type | Responsibility |
|---|---|---|
| `gasket-core` | lib | Stateless kernel: agent loop, message/event/tool types, built-in tools, LLM providers, extension API, JSONL storage. |
| `gasket-host` | lib | Reusable host: config, session management, permission policy, hook composition, context compaction, MCP client, subagent spawner, external tool bridge. |
| `gasket-ext` | lib | Optional in-process extensions (`hello` / `todo` / `search` / `permission_gate`). |
| `gasket-gateway` | bin | WebSocket gateway server: bridges the Vue frontend to the agent loop. |
| `gasket-cli` | bin | Interactive terminal REPL. |

The frontend (`web/`) is Vue 3 + Vite + Tauri 2 — one codebase for both browser and desktop.

For the full design — data flow, tool system, compaction algorithm, hook semantics, MCP integration — see [docs/architecture.md](./docs/architecture.md).

## Configuration

All backend config is via environment variables + `gasket/.env`. See [`.env.example`](./gasket/.env.example) for the complete reference, or [docs/usage.md](./docs/usage.md) for narrative guides.

Key groups:
- **LLM connection** (required): `GASKET_LLM_BASE_URL` / `GASKET_LLM_KEY` / `GASKET_LLM_MODEL` / `GASKET_LLM_API`
- **Gateway**: `GASKET_GATEWAY_PORT` (3000) / `GASKET_GATEWAY_MODE` / `GASKET_GATEWAY_STATIC_DIR`
- **Compaction**: `GASKET_CONTEXT_WINDOW` / `GASKET_COMPACT_THRESHOLD_PCT` / `GASKET_COMPACT_TARGET_PCT`
- **MCP**: `~/.gasket/mcp.json` (or `GASKET_MCP_CONFIG`)
- **Loop tunables**: `GASKET_MAX_TURNS` / `GASKET_MAX_TOOL_CALLS` / `GASKET_THINKING` / retry policy

## Docker

```bash
docker build -t gasket .
docker run -d -p 3000:3000 \
  -e GASKET_LLM_BASE_URL=https://api.deepseek.com/v1 \
  -e GASKET_LLM_KEY=sk-... \
  -e GASKET_LLM_MODEL=deepseek-chat \
  -e GASKET_LLM_API=openai \
  --name gasket gasket:latest
# Visit http://localhost:3000/
```

## Development

```bash
# Backend tests + lint
cd gasket && cargo test --all-features
cd gasket && cargo fmt --all -- --check
cd gasket && cargo clippy --all-features -- -D warnings

# Frontend
cd web && pnpm install && pnpm dev      # dev server (1420)
cd web && pnpm build                     # production build → dist/
cd web && pnpm tauri:dev                 # desktop dev
cd web && pnpm tauri:build               # desktop release (.dmg/.msi/.exe)
```

## Documentation

- [Architecture design](./docs/architecture.md) — internal structure, data flow, design decisions.
- [Usage guide](./docs/usage.md) — installation, configuration, deployment, troubleshooting.

## License

MIT — see [LICENSE](./LICENSE).
