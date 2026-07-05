<div align="center">

# Harness

**Run fleets of parallel coding agents with governance — orchestrate, police, review, and observe Claude Code / Codex at scale.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-orange.svg)](Cargo.toml)
[![CI](https://img.shields.io/github/actions/workflow/status/majiayu000/harness/ci.yml?branch=main&label=CI)](https://github.com/majiayu000/harness/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/majiayu000/harness?include_prereleases&label=release)](https://github.com/majiayu000/harness/releases)
[![Issues](https://img.shields.io/github/issues/majiayu000/harness?label=issues)](https://github.com/majiayu000/harness/issues)
[![Pull Requests](https://img.shields.io/github/issues-pr/majiayu000/harness?label=PRs)](https://github.com/majiayu000/harness/pulls)
[![Security Policy](https://img.shields.io/badge/security-policy-brightgreen.svg)](SECURITY.md)

![AI Agents](https://img.shields.io/badge/AI_Agents-222222.svg)
![Multi-Agent Orchestration](https://img.shields.io/badge/Multi--Agent-Orchestration-4c6fff.svg)
![Policy Engine](https://img.shields.io/badge/Policy-Engine-0f766e.svg)
![Observability](https://img.shields.io/badge/Observability-OTLP-7c3aed.svg)
![MCP Server](https://img.shields.io/badge/MCP-Server-2563eb.svg)

[Documentation](docs/) · [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md)

<img src="docs/images/harness-card.png" alt="Harness repository card" width="920" />

</div>

---

AI development is no longer one agent in one terminal — it is fleets of agents working in parallel across issues, branches, and repositories. The hard problems move up a level: who assigns the work, what each agent is allowed to do, who reviews the output, and what happens when a run goes wrong at 3 a.m.

Harness is a Rust-native control plane for that fleet. It wraps AI coding agents (Claude Code, Codex, Anthropic API) with structured lifecycle management, policy enforcement, and continuous feedback loops. Instead of replacing agents, it standardizes how they run, what they're allowed to do, and how their output is reviewed.

## Key Features

- **Fleet orchestration** — Run many agents in parallel with a unified task/thread/turn lifecycle; pluggable adapters for Claude Code CLI, Codex CLI, and Anthropic API
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

- Rust 1.88+
- Bun 1.1+ for release builds that embed the web dashboard. If `web/dist` is
  already built, release builds can reuse it.
- Postgres 14+. For local development, `scripts/dev-db.sh` starts the bundled
  Docker Compose Postgres service.
- A GitHub token for issue/PR automation. Use `gh auth login`, `GITHUB_TOKEN`,
  `GH_TOKEN`, or `server.github_token`.
- At least one agent runtime for autonomous execution:
  - [`codex`](https://github.com/openai/codex) CLI
  - [`claude`](https://docs.anthropic.com/en/docs/claude-code) CLI
  - Anthropic API key (for direct API adapter)

### Build

```bash
git clone https://github.com/majiayu000/harness.git
cd harness
cargo build --release -p harness-cli
```

`./start-server.sh` also builds the release CLI automatically when
`./target/release/harness` is missing, but running the build yourself keeps the
startup prerequisite explicit.

### Rust API Facade

For Rust consumers inside the repository or embedded integrations, `harness-api`
provides a curated stable import surface over the lower-level crates:

```rust
use std::path::Path;

use harness_api::core::SessionId;
use harness_api::exec::ExecPlan;
use harness_api::protocol::INTERNAL_ERROR;
use harness_api::sandbox::{SandboxMode, SandboxSpec};

let _session = SessionId::new();
let _plan = ExecPlan::from_spec("# Demo", Path::new(".")).expect("plan");
let _sandbox = SandboxSpec::new(SandboxMode::ReadOnly, ".");
let _code = INTERNAL_ERROR;
```

The facade groups the stable parts of `harness-core`, `harness-protocol`,
`harness-sandbox`, and `harness-exec` under one crate without forcing callers to
track internal crate layout changes.

### Database Setup

Harness requires Postgres 14+ (SQLite was removed in v0.x). Configure
`server.database_url` in your TOML config or set `HARNESS_DATABASE_URL` before
starting the server. When no config or environment database URL is present,
`./start-server.sh` uses the local development default
`postgres://harness:harness@localhost:5432/harness`. Migrations run
automatically on first connect.

**Option A — Docker Compose (recommended for local dev):**

```bash
# Start Postgres container (idempotent — safe to re-run)
bash scripts/dev-db.sh

# Then set `server.database_url = "postgres://harness:harness@localhost:5432/harness"`
# in your config file (for example `config/default.toml`).
```

**Option B — docker compose directly:**

```bash
docker compose up -d postgres
# Then set `server.database_url = "postgres://harness:harness@localhost:5432/harness"`
# in your config file.
```

**Option C — existing Postgres instance:**

Set `server.database_url` to any existing Postgres 14+ instance:

```toml
[server]
database_url = "postgres://user:password@host:5432/dbname"
```

**Running tests against a real database:**

```bash
createdb harness_test
HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test cargo test --workspace
```

Integration tests that require a database (e.g. `runtime_state_store`,
`thread_db`, `q_value_store`) skip automatically when no Harness database URL
is configured. Postgres-backed tests reject non-test database names by default;
use `harness_test`, a name ending in `_test`, or a name starting with `test_`.
For intentionally disposable databases with a different name, set
`HARNESS_ALLOW_NON_TEST_DATABASE_FOR_TESTS=1`.

**Harness-server validation ladder:**

```bash
# Routine server work: fast module and lightweight route path.
HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test scripts/test-server-fast.sh

# Full server DB, startup, recovery, route, and workflow profile.
HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test scripts/test-server-db.sh

# Final PR handoff: full workspace coverage.
HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test cargo test --workspace
```

`scripts/test-server-fast.sh` is the warm local feedback path for routine
`harness-server` changes once a test database URL is configured.
`scripts/test-server-db.sh` runs the full server suite with Cargo's default
parallelism. DB-backed tests must isolate state through unique test data paths
or explicit schema guards; tests that mutate true process-global state keep
their own precise `HOME` or environment lock.

When `cargo-nextest` is installed, the DB-capable server profile can also run
with process-level parallelism:

```bash
HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test cargo nextest run -p harness-server
```

### Run

**HTTP server:**

```bash
bash scripts/doctor.sh
./start-server.sh
curl http://127.0.0.1:9800/health
```

`scripts/doctor.sh` is non-mutating. It checks database URL resolution and
Postgres reachability, the release binary/build prerequisite, GitHub token
discovery, webhook secret readiness when webhook intake is enabled, local agent
CLI availability, required `WORKFLOW.md` runtime flags, port occupancy, and
unsafe non-local HTTP exposure before you start the server. Use
`scripts/doctor.sh --dry-run` when you want a report without a failing exit
code.

`./start-server.sh` selects `HARNESS_CONFIG`, `config/default.toml`,
`config/claude.toml`, a user config file, or built-in defaults in that order.
It verifies the HTTP port, starts the local Docker Compose Postgres service when
using the local fallback URL, loads `GITHUB_TOKEN` from `gh auth token` when
available, and builds `./target/release/harness` with
`cargo build --release -p harness-cli` if the release binary is missing.

`harness serve` persists its runtime log under `server.data_dir/logs/` as
`harness-serve-<startup-timestamp>-pid<PID>.log`. `/health` exposes a redacted
`runtime_logs.path_hint`, while `/api/operator-snapshot` includes the full
active path for operators.

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
project_root = "."
# Set this or HARNESS_API_TOKEN before exposing HTTP routes.
# api_token = "change-me"
# Local-dev escape hatch for tokenless HTTP operation:
# allow_unauthenticated = true

[agents]
default_agent = "auto"
# complexity_preferred_agents = ["codex", "claude"]
sandbox_mode = "danger-full-access"

[agents.claude]
cli_path = "claude"
default_model = "sonnet"

[agents.codex]
cli_path = "codex"

[agents.anthropic_api]
base_url = "https://api.anthropic.com"
default_model = "claude-sonnet-4-20250514"
max_tokens = 4096

[agents.review]
enabled = true
reviewer_agent = "codex"   # local review agent
max_rounds = 3
review_bot_auto_trigger = false

[gc]
max_drafts_per_run = 5
budget_per_signal_usd = 0.50
total_budget_usd = 5.0
draft_ttl_hours = 72

[observe]
log_retention_days = 90

[otel]
environment = "production"
exporter = "otlp-http"
# endpoint = "http://127.0.0.1:4318"
```

HTTP API authentication now fails closed by default. Starting `harness serve`
without `server.api_token` or `HARNESS_API_TOKEN` exits with an actionable
configuration error. For intentional tokenless local development, set
`server.allow_unauthenticated = true`; if both a token and the opt-in are set,
the token wins and bearer authentication stays enforced.

### Multi-Project Configuration

Register multiple projects in the config file. Each project gets its own worktree isolation, concurrency limits, and agent overrides.

```toml
[[projects]]
name = "harness"
root = "/path/to/harness"
default = true              # default project for API calls without project field
max_concurrent = 2          # max parallel tasks for this project

[[projects]]
name = "litellm-rs"
root = "/path/to/litellm-rs"
max_concurrent = 2
# default_agent = "auto"    # optional override; or set a registered agent name

[[projects]]
name = "vibeguard"
root = "/path/to/vibeguard"
max_concurrent = 1
```

CLI `--project name=path` flags merge with config entries (CLI overrides on conflict).

### Per-Project Overrides

Each project can have a `.harness/config.toml` in its root to override server defaults:

```toml
# /path/to/project/.harness/config.toml
[git]
base_branch = "develop"
remote = "upstream"
branch_prefix = "fix/"

[validation]
pre_commit = ["cargo fmt --all -- --check", "cargo check"]
timeout_secs = 120

[agent]
default = "auto"            # or set a registered agent name

[review]
enabled = true

[concurrency]
max_concurrent_tasks = 3
```

## HTTP REST API

### Task Management

```bash
# Submit a task by prompt
curl -X POST http://127.0.0.1:9800/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Add input validation to the API handler",
    "project": "/path/to/project"
  }'

# Submit a task by GitHub issue number
curl -X POST http://127.0.0.1:9800/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/path/to/project",
    "issue": 42,
    "prompt": "fix: handle edge case in parser"
  }'

# Submit an issue task but bypass triage/plan and go straight to implementation
curl -X POST http://127.0.0.1:9800/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/path/to/project",
    "issue": 42,
    "skip_triage": true
  }'

# Submit a task by PR number (for review/fix)
curl -X POST http://127.0.0.1:9800/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/path/to/project",
    "pr": 100
  }'

# Batch submit multiple tasks
curl -X POST http://127.0.0.1:9800/tasks/batch \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/path/to/project",
    "issues": [10, 11, 12]
  }'

# Get task status
curl http://127.0.0.1:9800/tasks/{task_id}

# List all tasks
curl http://127.0.0.1:9800/tasks

# Stream task output (SSE)
curl http://127.0.0.1:9800/tasks/{task_id}/stream
```

### Project Management

```bash
# List registered projects
curl http://127.0.0.1:9800/projects

# Register a new project at runtime
curl -X POST http://127.0.0.1:9800/projects \
  -H "Content-Type: application/json" \
  -d '{
    "id": "my-project",
    "root": "/path/to/project",
    "max_concurrent": 2
  }'

# Remove a project
curl -X DELETE http://127.0.0.1:9800/projects/my-project
```

### Dashboard

```bash
# Get aggregated status across all projects
curl http://127.0.0.1:9800/api/dashboard

# Response:
# {
#   "global": { "running": 3, "queued": 1, "done": 42, "failed": 2, "grade": "A" },
#   "projects": [
#     { "id": "harness", "root": "...", "tasks": { "running": 1, "queued": 0 } },
#     { "id": "litellm-rs", "root": "...", "tasks": { "running": 2, "queued": 1 } }
#   ]
# }
```

### Health

```bash
curl http://127.0.0.1:9800/health
```

The response includes a `runtime_logs` block with the logging state, retention
window, and a redacted `logs/<filename>` hint instead of the full absolute
path.

## Server Startup

`harness serve` can be started directly from a normal terminal. When product
behavior needs live verification from a Codex or Claude agent session, launch
the server with a sanitized environment so spawned agents do not inherit wrapper
variables from the parent process. Harness strips Claude-prefixed variables
before spawning child agents; Codex-prefixed variables are not stripped by the
adapter spawn path, so use `scripts/start-harness-codex-safe.sh` or an
equivalent sanitized launcher when starting from a Codex-owned session. For
long-running manual dogfood sessions, a standalone terminal is still useful
because the operator owns the process lifetime directly.

```bash
# Single project (backward compatible)
./start-server.sh

# Multi-project via config file (recommended)
./target/release/harness --config config/default.toml serve --transport http --port 9800

# Multi-project via CLI flags
./target/release/harness serve --transport http --port 9800 \
  --project harness=/path/to/harness \
  --project litellm=/path/to/litellm

# With GitHub token for auto-review
GITHUB_TOKEN=ghp_xxx ./target/release/harness --config config/default.toml serve --transport http --port 9800
```

Before using direct `./target/release/harness ... serve` commands, run
`scripts/doctor.sh --config <path>` or confirm the same prerequisites manually.
The direct binary commands do not start local Postgres or build the release
binary for you.

## Task Execution Flow

```
POST /tasks → TaskQueue → acquire semaphore (project + global)
    → create git worktree → agent executes in isolation
    → post-validator runs (cargo fmt, cargo check)
    → agent creates PR → Codex review (up to 3 rounds)
    → quality score → cleanup worktree → done
```

Each task runs in an isolated git worktree, so multiple agents can work on the same repo in parallel without conflicts.

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

Harness exposes 42 methods over JSON-RPC 2.0 (stdio, HTTP, or WebSocket):

| Category | Methods |
|---|---|
| Lifecycle | `initialize`, `initialized` |
| Threads | `thread/start`, `thread/resume`, `thread/fork`, `thread/list`, `thread/delete`, `thread/compact` |
| Turns | `turn/start`, `turn/steer`, `turn/cancel`, `turn/status`, `turn/respond_approval` |
| GC | `gc/run`, `gc/status`, `gc/drafts`, `gc/adopt`, `gc/reject` |
| Skills | `skill/create`, `skill/list`, `skill/get`, `skill/delete`, `skill/governance/view`, `skill/governance/history`, `skill/stale` |
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

## The Agent Infra Stack

This project is one layer of an open-source stack for running coding agents (Claude Code, Codex) as serious infrastructure. Each piece works independently; together they close the loop:

`harness` is the **Orchestrate** layer — where skills, rules, memory, and models come together into long-running agent runs.

| Layer | Project | What it does |
|---|---|---|
| Extend | [claude-skill-registry](https://github.com/majiayu000/claude-skill-registry) | Discover and search community Claude Code skills |
| Extend | [spellbook](https://github.com/majiayu000/spellbook) | Cross-runtime skills for Claude Code, Codex, and multi-agent workflows |
| Trust | [argus](https://github.com/majiayu000/argus) | Static install-time scanner for supply-chain attacks (npm / PyPI / crates.io) |
| Trust | [vibeguard](https://github.com/majiayu000/vibeguard) | Rules, hooks, and guards against hallucinated or unverified agent changes |
| Remember | [remem](https://github.com/majiayu000/remem) | Local-first persistent memory for Claude Code and Codex sessions |
| Orchestrate | [harness](https://github.com/majiayu000/harness) **◀ you are here** | Rust agent orchestration platform — rules, skills, GC, observability |
| Route | [litellm-rs](https://github.com/majiayu000/litellm-rs) | High-performance Rust AI gateway — 100+ LLM APIs via OpenAI format |
| Keep | [keepline](https://github.com/majiayu000/keepline) | Session command center — monitor, recover, never lose agent work |

---

## License

Licensed under the [MIT License](LICENSE).
