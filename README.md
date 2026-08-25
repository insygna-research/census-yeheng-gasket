# conga

> A lightweight, self-hostable personal AI assistant framework — written in Rust.

conga turns "an LLM agent that can call tools, stream output, manage sessions, and gate permissions" into a layered, reusable Rust workspace, with a Vue 3 web/desktop frontend.

## Features

- **Coding-agent system prompt** — tool discipline, verification duty, and surgical-change rules; the nearest `AGENTS.md`/`CLAUDE.md` is injected, and every turn prepends an environment snapshot (UTC date, platform, `git status`, `git diff --stat`).
- **Stateless agent loop** — the core reasoning loop is a pure function; all state lives in the host layer. Inject any LLM via the `StreamFn` trait.
- **9 built-in tools** — `read` / `write` / `edit` / `bash` / `grep` / `list` / `fetch` / `todo` / `spawn_subagents`, each with a risk level (`Low` / `Medium` / `High`).
- **Persistent shell** — `bash` keeps one shell per session: `cd`, exported env vars, and activated virtualenvs survive across calls; `run_in_background` detaches long commands into pollable log files.
- **Multi-hunk edits** — one `edit` call applies several hunks atomically (all-or-nothing); approval dialogs render a real diff preview, not raw JSON.
- **Mid-turn steering** — messages sent while a turn is running are queued (`queued`) and injected as real User messages before the next LLM call — not rejected.
- **Subagents with receipts** — parallel sub-agent loops persist their own `events.jsonl` under the parent session and return their FULL transcripts (plus log paths) to the parent; an optional fast model (`CONGA_FAST_LLM_*`) routes them cheaper.
- **Hook chain & permissions** — `before_tool_call` (async, can block/modify) + `after_tool_call` (sync, can rewrite results). Four permission modes: `suggest` / `auto-edit` / `full-auto` / `plan` (read-only planning: mutating tools are hard-blocked and a plan directive rides each turn's user message).
- **Context compaction** — token-aware, turn-boundary-safe, pins the original task message and head-truncates old tool results; the on-disk event log stays append-only and complete.
- **MCP client** — connect [Model Context Protocol](https://modelcontextprotocol.io) tool servers (stdio + Streamable HTTP). Reuse existing Claude-Desktop-style `mcp.json` configs.
- **Two frontends, one host** — a terminal REPL (`conga` CLI) and a WebSocket gateway (`conga-gateway`) both drive the same `Host::run_turn`. The Vue 3 frontend runs as a browser app or a Tauri desktop app from one codebase.
- **Crash-safe event log** — every session is an append-only `events.jsonl`: each side effect (assistant message, tool result) hits disk as it happens, so a crashed/aborted/errored turn keeps everything that already occurred. Torn-tail self-healing drops a truncated last line on crash; mid-file corruption reports with line numbers; unknown event variants fail closed. A `GET /api/sessions/{key}/messages` REST endpoint derives the transcript from disk on demand.
- **Rate-limit-aware retry** — 429 responses back off on a longer schedule with jitter; partial-set `CONGA_FAST_LLM_*` typos fail loud at startup.
- **Headless `conga exec`** — one-shot turns for CI/scripts: same host wiring as the REPL, NDJSON event stream on stdout (`--json`, the same wire schema as the gateway), CI-friendly exit codes (`0` done / `1` turn error / `130` aborted / `2` usage), task text via argument or stdin (`-`).
- **Tool-name conflict detection** — assembled tool sets dedup by name with a warning (first registration wins: built-in → ext → external → MCP).

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
- **Gateway**: `CONGA_GATEWAY_PORT` (3000) / `CONGA_GATEWAY_MODE` / `CONGA_GATEWAY_STATIC_DIR` / `CONGA_GATEWAY_TOKEN` (auth for `/ws` + `/api/*`)

## Docker

```bash
docker build -t conga .
docker run -d -p 3000:3000 \
  -e CONGA_LLM_BASE_URL=https://api.deepseek.com/v1 \
  -e CONGA_LLM_KEY=sk-... \
  -e CONGA_LLM_MODEL=deepseek-chat \
  -e CONGA_LLM_API=openai \
  -e CONGA_GATEWAY_TOKEN=change-me \
  --name conga conga:latest
# Visit http://localhost:3000/ and enter the token in Settings → Connection.
```

All `/ws` and `/api/*` requests require the gateway token (`Authorization: Bearer <t>` or `?token=<t>`); static assets are exempt. Without `CONGA_GATEWAY_TOKEN`, a random token is generated on first start and stored in `~/.conga/gateway_token` (0600). The desktop app uses in-process IPC and needs no token.

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
