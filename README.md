# conga

> A lightweight, self-hostable personal AI assistant framework — written in Rust.

conga turns "an LLM agent that can call tools, stream output, manage sessions, and gate permissions" into a layered, reusable Rust workspace, with a Vue 3 web/desktop frontend.

## Features

- **Stateless agent loop** — the core reasoning loop is a pure function; all state lives in the host layer. Inject any LLM via the `StreamFn` trait.
- **8 built-in tools** — `read` / `write` / `edit` / `bash` / `grep` / `list` / `fetch` / `spawn_subagents`, each with a risk level (`Low` / `Medium` / `High`).
- **Hook chain & permissions** — `before_tool_call` (async, can block/modify) + `after_tool_call` (sync, can rewrite results). Three permission modes: `suggest` / `auto-edit` / `full-auto`.
- **Context compaction** — token-aware, turn-boundary-safe (never splits a `tool_call` from its `tool_result`), with provider-reported usage. Memory-only — the on-disk event log is always append-only and complete; the token budget is restored from the log tail each turn, so compaction survives restarts.
- **MCP client** — connect [Model Context Protocol](https://modelcontextprotocol.io) tool servers (stdio + Streamable HTTP). Reuse existing Claude-Desktop-style `mcp.json` configs.
- **Subagent orchestration** — `spawn_subagents` tool fans out parallel sub-agent loops with real-time event streaming to the frontend.
- **Two frontends, one host** — a terminal REPL (`conga` CLI) and a WebSocket gateway (`conga-gateway`) both drive the same `Host::run_turn`. The Vue 3 frontend runs as a browser app or a Tauri desktop app from one codebase.
- **Crash-safe event log** — every session is an append-only `events.jsonl`: each side effect (assistant message, tool result) hits disk as it happens, so a crashed/aborted/errored turn keeps everything that already occurred. Torn-tail self-healing drops a truncated last line on crash; mid-file corruption reports with line numbers; unknown event variants fail closed. A `GET /api/sessions/{key}/messages` REST endpoint derives the transcript from disk on demand.

## Quick start (5 minutes)

```bash
# 1) Clone
git clone https://github.com/YeHeng/conga.git
cd conga

# 2) Configure the LLM (three required vars)
cp conga/.env.example conga/.env
#   Edit conga/.env:
#     CONGA_LLM_BASE_URL=https://api.deepseek.com/v1
#     CONGA_LLM_KEY=your-key
#     CONGA_LLM_MODEL=deepseek-chat
#     CONGA_LLM_API=openai        # or anthropic

# 3) Start the backend gateway (serves the frontend too, once built)
cd conga && cargo run --release --bin conga-gateway

# 4) In another terminal, start the web frontend (dev mode, port 1420)
cd web && pnpm install && pnpm dev
#   Open http://localhost:1420
```

Prefer the terminal? Skip the frontend:

```bash
cd conga && cargo run --release --bin conga
```

## Architecture

conga is a Cargo workspace with 5 crates, in a strict `core → host → frontends` layering:

| Crate | Type | Responsibility |
|---|---|---|
| `conga` | lib | Stateless kernel: agent loop, message/event/tool types, built-in tools, LLM providers, extension API, event-log storage. |
| `conga-host` | lib | Reusable host: config, session management, permission policy, hook composition, context compaction, MCP client, subagent spawner, external tool bridge. |
| `conga-ext` | lib | Optional in-process extensions (`hello` / `todo` / `search` / `permission_gate`). |
| `conga-gateway` | bin | WebSocket gateway server: bridges the Vue frontend to the agent loop, plus a REST transcript endpoint (`GET /api/sessions/{key}/messages`) that derives history from the on-disk event log. |
| `conga-cli` | bin | Interactive terminal REPL. |

The frontend (`web/`) is Vue 3 + Vite + Tauri 2 — one codebase for both browser and desktop.

For the full design — data flow, tool system, compaction algorithm, hook semantics, MCP integration — see [docs/architecture.md](./docs/architecture.md).

## Configuration

All backend config is via environment variables + `conga/.env`. See [`.env.example`](./conga/.env.example) for the complete reference, or [docs/usage.md](./docs/usage.md) for narrative guides.

Key groups:
- **LLM connection** (required): `CONGA_LLM_BASE_URL` / `CONGA_LLM_KEY` / `CONGA_LLM_MODEL` / `CONGA_LLM_API`
- **Gateway**: `CONGA_GATEWAY_PORT` (3000) / `CONGA_GATEWAY_MODE` / `CONGA_GATEWAY_STATIC_DIR`
- **Compaction**: `CONGA_CONTEXT_WINDOW` / `CONGA_COMPACT_THRESHOLD_PCT` / `CONGA_COMPACT_TARGET_PCT`
- **MCP**: `~/.conga/mcp.json` (or `CONGA_MCP_CONFIG`)
- **Loop tunables**: `CONGA_MAX_TURNS` / `CONGA_MAX_TOOL_CALLS` / `CONGA_THINKING` / retry policy

## Docker

```bash
docker build -t conga .
docker run -d -p 3000:3000 \
  -e CONGA_LLM_BASE_URL=https://api.deepseek.com/v1 \
  -e CONGA_LLM_KEY=sk-... \
  -e CONGA_LLM_MODEL=deepseek-chat \
  -e CONGA_LLM_API=openai \
  --name conga conga:latest
# Visit http://localhost:3000/
```

## Development

```bash
# Backend tests + lint
cd conga && cargo test --all-features
cd conga && cargo fmt --all -- --check
cd conga && cargo clippy --all-features -- -D warnings

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
