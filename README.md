<div align="center">

# Harness

**An orchestration layer for AI coding agents — govern, observe, and improve agent workflows at scale.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.75+-orange.svg)](https://www.rust-lang.org)
[![CI](https://img.shields.io/github/actions/workflow/status/majiayu000/harness/ci.yml?branch=main&label=CI)](https://github.com/majiayu000/harness/actions/workflows/ci.yml)

[Documentation](docs/) · [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md)

</div>

---

Harness is a Rust-native platform that wraps AI coding agents (Claude Code, Codex, Anthropic API) with structured lifecycle management, policy enforcement, and continuous feedback loops. Instead of replacing agents, it standardizes how they run, what they're allowed to do, and how their output is reviewed.

## Key Features

- **Multi-agent orchestration** — Pluggable adapters for Claude Code CLI, Codex CLI, and Anthropic API with unified task/thread/turn lifecycle
- **Independent agent review** — Automatic cross-agent code review between implementation and GitHub review, preventing self-review by architecture
- **Policy engine** — Starlark-based execution policies with hardened parser dialect (no `load`/`def`/`lambda`) for sandboxed rule evaluation
- **Signal-driven GC** — Detects repeated warnings, chronic blockers, and hot files; generates and adopts remediation drafts within configurable budgets
- **GitHub webhook automation** — HMAC-SHA256 verified webhooks parse `@harness` mentions to trigger tasks from issue comments and PR reviews
- **OpenTelemetry export** — Native OTLP/HTTP/gRPC traces and metrics with async-safe transport for signal-handler contexts
- **MCP server mode** — JSON-RPC stdio interface exposing harness tools as an MCP-compatible server
- **CI/CD GitHub Action** — Workspace-bound execution with path traversal protection and privilege enforcement

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        Harness CLI                          │
│              serve · exec · gc · rule · skill               │
├──────────┬──────────┬───────────────────────────────────────┤
│  stdio   │   HTTP   │  WebSocket   │  MCP Server  │ Webhook│
├──────────┴──────────┴──────────┴───┴──────────────┴────────┤
│                    JSON-RPC Router (30 methods)             │
├────────────┬─────────────┬────────────┬────────────────────┤
│   Threads  │    Tasks    │   Turns    │    ExecPlans       │
├────────────┴─────────────┴────────────┴────────────────────┤
│  harness-agents    │  harness-rules   │  harness-skills    │
│  (Claude/Codex/API)│  (Starlark exec) │  (discovery/dedup) │
├────────────────────┼──────────────────┼────────────────────┤
│  harness-gc        │  harness-observe │  harness-exec      │
│  (signal/drafts)   │  (events/OTLP)  │  (plan lifecycle)  │
├────────────────────┴──────────────────┴────────────────────┤
│                    harness-core                             │
│          config · prompts · domain types · traits           │
├────────────────────────────────────────────────────────────┤
│                    harness-protocol                         │
│       JSON-RPC envelopes · method definitions · codecs      │
└────────────────────────────────────────────────────────────┘
        ▼               ▼                ▼
   Claude Code CLI   Codex CLI    Anthropic API
```

## Quick Start

### Prerequisites

- Rust 1.75+
- At least one agent runtime:
  - [`claude`](https://docs.anthropic.com/en/docs/claude-code) CLI (default)
  - [`codex`](https://github.com/openai/codex) CLI
  - Anthropic API key (for direct API adapter)

### Build

```bash
git clone https://github.com/majiayu000/harness.git
cd harness
cargo build
```

### Run

**HTTP server:**

```bash
cargo run -p harness-cli -- serve --transport http --port 9800
curl http://127.0.0.1:9800/health
```

**Stdio (for MCP integration):**

```bash
cargo run -p harness-cli -- serve --transport stdio
```

**One-shot execution:**

```bash
cargo run -p harness-cli -- exec "Fix the failing test in src/lib.rs"
```

### Common Workflows

```bash
# Task management
curl -X POST http://127.0.0.1:9800/tasks \
  -H "Content-Type: application/json" \
  -d '{"prompt": "Add input validation to the API handler"}'

# Rule engine
cargo run -p harness-cli -- rule load .
cargo run -p harness-cli -- rule check .

# GC cycle — detect signals and generate remediation drafts
cargo run -p harness-cli -- gc run .

# Skill discovery
cargo run -p harness-cli -- skill list

# ExecPlan lifecycle
cargo run -p harness-cli -- plan init ./spec.md
cargo run -p harness-cli -- plan status ./exec-plan-<id>.md
```

## Configuration

All settings are declarative TOML. Pass `--config <path>` or use the defaults in [`config/default.toml`](config/default.toml).

```toml
[server]
transport = "stdio"
http_addr = "127.0.0.1:9800"
data_dir = "~/.local/share/harness"

[agents]
default_agent = "claude"

[agents.review]
enabled = true
reviewer_agent = "codex"   # independent reviewer != implementor
max_rounds = 3

[gc]
max_drafts_per_run = 5
budget_per_signal_usd = 0.50

[otel]
environment = "production"
exporter = "otlp-http"
# endpoint = "http://127.0.0.1:4318"
```

## Workspace Crates

| Crate | Purpose |
|---|---|
| `harness-core` | Shared domain types, config, prompts, agent/interceptor traits |
| `harness-protocol` | JSON-RPC method definitions, envelopes, notifications, codecs |
| `harness-server` | App Server runtime (HTTP + stdio + WebSocket), routing, handlers, task/thread stores |
| `harness-agents` | Agent adapters (Claude CLI, Codex CLI, Anthropic API) and registry |
| `harness-gc` | Signal detection and draft remediation generation/adoption |
| `harness-rules` | Rule loading/parsing, Starlark execution policy engine |
| `harness-skills` | Skill discovery, deduplication, search, and persistence |
| `harness-exec` | ExecPlan model plus Markdown serialization/deserialization |
| `harness-observe` | Event storage, quality grading, health/stat aggregation, OTLP export |
| `harness-cli` | `harness` binary with serve/exec/gc/rule/skill/plan commands |

## JSON-RPC API

Harness exposes 38 methods over JSON-RPC 2.0 (stdio, HTTP, or WebSocket):

| Category | Methods |
|---|---|
| Lifecycle | `initialize`, `initialized` |
| Threads | `thread/start`, `thread/resume`, `thread/fork`, `thread/list`, `thread/delete`, `thread/compact` |
| Turns | `turn/start`, `turn/steer`, `turn/cancel`, `turn/status` |
| GC | `gc/run`, `gc/status`, `gc/drafts`, `gc/adopt`, `gc/reject` |
| Skills | `skill/create`, `skill/list`, `skill/get`, `skill/delete` |
| Rules | `rule/load`, `rule/check` |
| ExecPlan | `exec_plan/init`, `exec_plan/update`, `exec_plan/status` |
| Observability | `event/log`, `event/query`, `metrics/collect`, `metrics/query` |
| Classification | `task/classify`, `learn/rules`, `learn/skills` |
| Health | `health/check`, `stats/query`, `agent/list` |
| VibeGuard | `preflight`, `cross_review` |

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for development setup and PR guidelines.

## Security

See [`SECURITY.md`](SECURITY.md) for vulnerability reporting.

## License

Licensed under the [MIT License](LICENSE).
