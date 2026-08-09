<div align="center">

# Harness

**Ship code with a fleet of AI agents you can actually trust — orchestrated, policed, reviewed, and observable.**

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

## Install

Build from source (no prebuilt binaries or Homebrew formula yet):

```bash
git clone https://github.com/majiayu000/harness.git
cd harness
cargo build --release -p harness-cli
# binary at ./target/release/harness
```

Requires Rust 1.88+. A fresh release build also requires Bun 1.1+ because it
embeds the web dashboard; if `web/dist` is already built, the release build can
reuse it without Bun. Postgres and an API authentication token are only needed
for the server / fleet features below; a GitHub token is additionally needed
for GitHub integration.

## Quickstart: run one agent task

Install one local coding runtime on your `PATH`: either
[`codex`](https://github.com/openai/codex) or
[`claude`](https://docs.anthropic.com/en/docs/claude-code). Run Harness as an
unprivileged OS user: `--drop-sudo` defaults to `true`, so `harness exec`
rejects root and sudo environments. Only pass `--drop-sudo=false` when elevated
execution is deliberate.

On Linux, the default `workspace-write` sandbox also requires
`harness-landlock` or [`bwrap`](https://github.com/containers/bubblewrap) on
`PATH`; install your distribution's Bubblewrap package if you do not have the
Landlock helper. A host-tier `danger-full-access` agent with scoped permissions
and an empty network allowlist requires `bwrap` specifically: Landlock cannot
provide network-only isolation while leaving filesystem access unrestricted.
Harness reports that host tier as unavailable during startup health probing and
refuses matching dispatches. Other Linux sandbox combinations continue to
accept either helper.

```bash
# With Codex CLI (Linux or macOS)
./target/release/harness exec --agent codex \
  "Summarize the public API exposed by crates/harness-core/src/lib.rs"

# Or with Claude Code CLI on Linux
./target/release/harness exec --agent claude \
  "Summarize the public API exposed by crates/harness-core/src/lib.rs"

# Claude Code on macOS cannot run under the Seatbelt workspace-write sandbox
./target/release/harness exec --agent claude --sandbox-mode danger-full-access \
  "Summarize the public API exposed by crates/harness-core/src/lib.rs"
```

Harness runs the explicitly selected coding agent against the current directory
and prints the agent's final response to stdout. The default is
`workspace-write`. The macOS Claude exception grants the agent unrestricted
filesystem and process access; use it only in a trusted repository and review
the resulting changes.

The `anthropic-api` adapter is for text generation: it sends the prompt without
repository context or tools, so it cannot inspect or modify the project in this
coding example.

The `opencode` adapter runs OpenCode (`opencode run` / `opencode acp`), a
provider-agnostic coding agent. On macOS it must run outside the Seatbelt
sandbox like Claude Code: `--agent opencode --sandbox-mode danger-full-access`.

Useful flags: `--project <dir>`, `--agent claude|codex|anthropic-api|opencode`,
`--model <id>`, `--sandbox-mode <mode>`, `--output-file result.md`. Supported
sandbox modes are `read-only`, `read-only-with-network`, `workspace-write`, and
`danger-full-access`.

## Level up: the fleet control plane

For parallel agents, task queues, cross-agent review, and the web dashboard,
start the server (needs Postgres 14+, an API bearer token, and Docker Engine
with the Docker Compose v2.1+ plugin for the bundled database). The copy-paste
token generation below also needs the OpenSSL CLI. The Codex-safe launcher
additionally requires `curl` and `lsof`.

In a normal standalone terminal, start Postgres and the foreground server:

```bash
# Terminal 1
HARNESS_API_TOKEN="$(openssl rand -hex 32)" &&
  test -n "$HARNESS_API_TOKEN" &&
  export HARNESS_API_TOKEN &&
  printf '\nCopy this command into every client terminal:\n  export HARNESS_API_TOKEN=%s\n\n' \
    "$HARNESS_API_TOKEN" &&
  bash scripts/dev-db.sh &&
  ./start-server.sh
```

From a Codex-owned session, use the sanitized launcher instead; it removes
wrapper variables that can confuse child Codex agents. This environment-based
quickstart selects its background `nohup` path so the database and authentication
settings reach the server even when another tmux server already exists:

```bash
HARNESS_API_TOKEN="$(openssl rand -hex 32)" &&
  test -n "$HARNESS_API_TOKEN" &&
  export HARNESS_API_TOKEN &&
  printf '\nCopy this command into every client terminal:\n  export HARNESS_API_TOKEN=%s\n\n' \
    "$HARNESS_API_TOKEN" &&
  bash scripts/dev-db.sh &&
  export HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness &&
  export HARNESS_DATABASE_POOL_MAX_CONNECTIONS=16 &&
  export HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS=60 &&
  HARNESS_STARTER_NO_TMUX=1 bash scripts/start-harness-codex-safe.sh
```

For the standalone path, `start-server.sh` remains in the foreground, so check
it from a second terminal. First paste the exact
`export HARNESS_API_TOKEN=...` command printed by Terminal 1, then verify both
the unauthenticated health endpoint and an authenticated API endpoint:

```bash
# Terminal 2
# Paste the export command printed by Terminal 1 before running these commands.
curl http://127.0.0.1:9800/health
curl http://127.0.0.1:9800/api/dashboard \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}"
```

Open `http://127.0.0.1:9800/` and enter the same token when the dashboard
prompts for it. The token grants API access; treat the printed command as a
secret and do not commit or share it. Shell environments are not shared between
terminals or persisted across restarts; after restarting, use the newly printed
export command.

Full server setup, configuration, and workflows are covered in
[Quick Start](#quick-start) below.

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

CLI install and one-shot execution are covered in [Install](#install) and
[Quickstart](#quickstart-run-one-agent-task) above. This section covers the
server and development setup.

### Server prerequisites

- Bun 1.1+ for release builds that embed the web dashboard. If `web/dist` is
  already built, release builds can reuse it.
- Postgres 14+. For local development, `scripts/dev-db.sh` starts the bundled
  Postgres service and requires Docker Engine with the Docker Compose v2.1+
  plugin (`docker compose`, not legacy `docker-compose` v1).
- A GitHub token for issue/PR automation. Use `gh auth login`, `GITHUB_TOKEN`,
  `GH_TOKEN`, or `server.github_token`.

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

# Optional nextest runner for the same DB-capable profile.
HARNESS_SERVER_TEST_RUNNER=nextest HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test scripts/test-server-db.sh

# Final PR handoff: full workspace coverage.
HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test cargo test --workspace
```

`scripts/test-server-fast.sh` is the warm local feedback path for routine
`harness-server` changes once a test database URL is configured.
`scripts/test-server-db.sh` runs the full server suite with default test
parallelism. DB-backed tests rely on unique temporary data directories or
explicit `TestSchemaGuard` schemas. Tests that mutate true process-global state,
such as `HOME`, keep their own named locks.

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
active path for operators. Startup cleanup removes matching runtime logs older
than `observe.log_retention_days` and prunes the oldest extra matching logs
beyond `observe.log_retention_max_files`; set the max-files value to `0` for
age-only retention.

**Stdio (for MCP integration):**

```bash
cargo run -p harness-cli -- serve --transport stdio
```

**One-shot execution:**

```bash
cargo run -p harness-cli -- exec \
  "Summarize the public API exposed by crates/harness-core/src/lib.rs"
```

### Common Workflows

The protected HTTP examples assume `HARNESS_API_TOKEN` is exported in the
client shell as shown in the server quickstart above.

```bash
# Workflow-runtime submission
curl -X POST http://127.0.0.1:9800/api/workflows/runtime/submissions \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}" \
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
# Claude enforces Standard tools. Set "full" only for an explicit unrestricted opt-up.
capability_profile = "standard"

[isolation]
default_tier = "container"
# Exact hosts only. Scoped mode with an empty list denies agent networking.
# Linux allowlisted tasks require the container tier; macOS also supports host.
network_allowlist = ["github.com", "api.github.com", "api.openai.com", "api.anthropic.com"]

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
log_retention_max_files = 30
log_retention_days = 90

[otel]
environment = "production"
exporter = "otlp-http"
# endpoint = "http://127.0.0.1:4318"
```

With `default_agent = "auto"`, Claude is the first registered CLI agent. Start
Harness with `ANTHROPIC_API_KEY` set when using the container tier. Harness
forwards that provider credential to the Claude container by environment
variable name; the value is kept out of Docker arguments. Other ambient
operator secrets remain filtered.

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
max_turns = 20
```

## HTTP REST API

Except for `/health`, the examples below assume `HARNESS_API_TOKEN` is exported
in the client shell and send it as a bearer token.

### Workflow Runtime Submissions

```bash
# Submit work by prompt
curl -X POST http://127.0.0.1:9800/api/workflows/runtime/submissions \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "Add input validation to the API handler",
    "project": "/path/to/project"
  }'

# Submit work by GitHub issue number
curl -X POST http://127.0.0.1:9800/api/workflows/runtime/submissions \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/path/to/project",
    "issue": 42,
    "prompt": "fix: handle edge case in parser"
  }'

# Submit an issue but bypass triage/plan and go straight to implementation
curl -X POST http://127.0.0.1:9800/api/workflows/runtime/submissions \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/path/to/project",
    "issue": 42,
    "skip_triage": true
  }'

# Submit a PR for review/fix
curl -X POST http://127.0.0.1:9800/api/workflows/runtime/submissions \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/path/to/project",
    "pr": 100
  }'

# Submit multiple issues (one durable submission per request)
for issue in 10 11 12; do
  curl -X POST http://127.0.0.1:9800/api/workflows/runtime/submissions \
    -H "Authorization: Bearer ${HARNESS_API_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "{\"project\":\"/path/to/project\",\"issue\":$issue}"
done

# Get submission status
curl http://127.0.0.1:9800/api/workflows/runtime/submissions/{submission_id} \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}"

# List submissions
curl http://127.0.0.1:9800/api/workflows/runtime/submissions \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}"

# Stream submission output (SSE)
curl http://127.0.0.1:9800/api/workflows/runtime/submissions/{submission_id}/stream \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}"
```

### Project Management

```bash
# List registered projects
curl http://127.0.0.1:9800/projects \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}"

# Register a new project at runtime
curl -X POST http://127.0.0.1:9800/projects \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "my-project",
    "root": "/path/to/project",
    "max_concurrent": 2
  }'

# Remove a project
curl -X DELETE http://127.0.0.1:9800/projects/my-project \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}"
```

### Dashboard

```bash
# Get aggregated status across all projects
curl http://127.0.0.1:9800/api/dashboard \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}"

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
window, max-files cap, and a redacted `logs/<filename>` hint instead of the full
absolute path.

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

## Workflow Runtime Execution Flow

```
POST /api/workflows/runtime/submissions → persist workflow + implementation command
    → runtime dispatcher creates a job → worker acquires the project queue permit
    → create git worktree → agent executes in isolation
    → validate and review → persist terminal workflow evidence
```

Each implementation runs in an isolated git worktree. The workflow runtime is
the scheduling and lifecycle authority.

## Workspace Crates

| Crate | Purpose |
|---|---|
| `harness-core` | Shared domain types, config, prompts, agent/interceptor traits |
| `harness-protocol` | JSON-RPC method definitions, envelopes, notifications, codecs |
| `harness-server` | App Server runtime (HTTP + stdio + WebSocket), routing, handlers, and workflow-runtime integration |
| `harness-agents` | Agent adapters (Claude CLI, Codex CLI, Anthropic API) and registry |
| `harness-gc` | Signal detection and draft remediation generation/adoption |
| `harness-rules` | Rule loading/parsing, Starlark execution policy engine |
| `harness-skills` | Skill discovery, deduplication, search, and persistence |
| `harness-exec` | ExecPlan model plus Markdown serialization/deserialization |
| `harness-observe` | Event storage, quality grading, health/stat aggregation, OTLP export |
| `harness-cli` | `harness` binary with serve/exec/gc/rule/skill/plan commands |

## JSON-RPC API

Harness exposes 32 methods over JSON-RPC 2.0 (stdio, HTTP, or WebSocket):

| Category | Methods |
|---|---|
| Lifecycle | `initialize`, `initialized` |
| GC | `gc/run`, `gc/status`, `gc/drafts`, `gc/adopt`, `gc/reject` |
| Skills | `skill/create`, `skill/list`, `skill/get`, `skill/delete`, `skill/governance/view`, `skill/governance/history`, `skill/stale` |
| Rules | `rule/load`, `rule/check` |
| ExecPlan | `exec_plan/init`, `exec_plan/update`, `exec_plan/status` |
| Observability | `event/log`, `event/query`, `metrics/collect`, `metrics/query` |
| Context | `context/preview` |
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
