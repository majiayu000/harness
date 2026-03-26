# Harness Usage Guide

## Installation

```bash
git clone https://github.com/majiayu000/harness.git
cd harness
cargo build --release
```

The binary is at `./target/release/harness`.

## Server Startup

> **Important:** Never start the server from within Claude Code or other agent sessions. The `CLAUDECODE` and `CLAUDE_CODE_ENTRYPOINT` environment variables propagate to spawned agents and cause SIGTRAP crashes. Always use a standalone terminal.

### Single Project

```bash
./target/release/harness serve \
  --transport http \
  --port 9800 \
  --project-root /path/to/your/project
```

### Multi-Project via Config File (Recommended)

Create or edit `config/default.toml`:

```toml
[server]
transport = "stdio"
http_addr = "127.0.0.1:9800"
data_dir = "~/.local/share/harness"

[agents]
default_agent = "codex"
sandbox_mode = "danger-full-access"

[agents.claude]
cli_path = "claude"
default_model = "sonnet"

[agents.codex]
cli_path = "codex"

[agents.review]
enabled = true
reviewer_agent = "codex"
max_rounds = 3

[gc]
max_drafts_per_run = 5
budget_per_signal_usd = 0.50
total_budget_usd = 5.0
draft_ttl_hours = 72

[observe]
log_retention_days = 90

[otel]
environment = "development"
exporter = "disabled"

[[projects]]
name = "my-app"
root = "/path/to/my-app"
default = true
max_concurrent = 2

[[projects]]
name = "my-lib"
root = "/path/to/my-lib"
max_concurrent = 1
default_agent = "codex"
```

Start with:

```bash
./target/release/harness serve \
  --transport http \
  --port 9800 \
  --config config/default.toml
```

### Multi-Project via CLI Flags

```bash
./target/release/harness serve \
  --transport http \
  --port 9800 \
  --project my-app=/path/to/my-app \
  --project my-lib=/path/to/my-lib \
  --default-project my-app
```

CLI `--project` flags merge with config `[[projects]]` entries. CLI overrides config on name conflict.

### With GitHub Token

Enable auto-review bot comments on PRs:

```bash
GITHUB_TOKEN=ghp_xxx ./target/release/harness serve \
  --transport http \
  --port 9800 \
  --config config/default.toml
```

### With Anthropic API Key

Enable the direct Anthropic API agent:

```bash
ANTHROPIC_API_KEY=sk-ant-xxx ./target/release/harness serve \
  --transport http \
  --port 9800 \
  --config config/default.toml
```

## Submitting Tasks

### By Prompt

For ad-hoc work without a GitHub issue:

```bash
curl -X POST http://127.0.0.1:9800/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/path/to/project",
    "prompt": "Add input validation to the user registration endpoint. Check email format, password strength (min 8 chars), and sanitize the username field.",
    "description": "feat: input validation for registration"
  }'
```

Response:

```json
{ "status": "running", "task_id": "a1b2c3d4-..." }
```

### By GitHub Issue

The agent reads the issue title and body, then implements it:

```bash
curl -X POST http://127.0.0.1:9800/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/path/to/project",
    "issue": 42,
    "description": "fix: handle edge case in parser"
  }'
```

### By Pull Request

For reviewing or fixing an existing PR:

```bash
curl -X POST http://127.0.0.1:9800/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/path/to/project",
    "pr": 100
  }'
```

### Batch Submit

Submit multiple issues at once:

```bash
curl -X POST http://127.0.0.1:9800/tasks/batch \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/path/to/project",
    "issues": [10, 11, 12, 13]
  }'
```

Response:

```json
[
  { "task_id": "...", "status": "running" },
  { "task_id": "...", "status": "queued" },
  { "task_id": "...", "status": "queued" },
  { "task_id": "...", "status": "queued" }
]
```

Tasks respect concurrency limits — excess tasks are queued.

## Monitoring

### Dashboard

Get a snapshot of all projects and tasks:

```bash
curl -s http://127.0.0.1:9800/api/dashboard | python3 -m json.tool
```

```json
{
  "global": {
    "running": 3,
    "queued": 1,
    "done": 42,
    "failed": 2,
    "grade": "A",
    "max_concurrent": 4,
    "latest_pr": "https://github.com/owner/repo/pull/123"
  },
  "projects": [
    {
      "id": "my-app",
      "root": "/path/to/my-app",
      "tasks": { "running": 2, "queued": 1 }
    },
    {
      "id": "my-lib",
      "root": "/path/to/my-lib",
      "tasks": { "running": 1, "queued": 0 }
    }
  ]
}
```

### Task Status

```bash
# Single task
curl http://127.0.0.1:9800/tasks/{task_id}

# All tasks
curl http://127.0.0.1:9800/tasks
```

### SSE Streaming

Stream real-time output from a running task:

```bash
curl -N http://127.0.0.1:9800/tasks/{task_id}/stream
```

### Health Check

```bash
curl http://127.0.0.1:9800/health
```

## Project Management API

### List Projects

```bash
curl http://127.0.0.1:9800/api/projects
```

### Register a Project at Runtime

No server restart needed:

```bash
curl -X POST http://127.0.0.1:9800/api/projects \
  -H "Content-Type: application/json" \
  -d '{
    "id": "new-project",
    "root": "/path/to/new-project",
    "max_concurrent": 2,
    "default_agent": "codex",
    "active": true
  }'
```

### Remove a Project

```bash
curl -X DELETE http://127.0.0.1:9800/api/projects/new-project
```

## Configuration Reference

### `[server]`

| Field | Default | Description |
|-------|---------|-------------|
| `transport` | `"stdio"` | Transport protocol: `stdio`, `http`, or `web_socket` |
| `http_addr` | `"127.0.0.1:9800"` | HTTP listen address |
| `data_dir` | `"~/.local/share/harness"` | Data directory for SQLite databases |
| `project_root` | `"."` | Default project root (single-project mode) |
| `github_webhook_secret` | — | HMAC-SHA256 secret for GitHub webhook verification |
| `notification_broadcast_capacity` | `256` | Internal notification channel capacity |

### `[agents]`

| Field | Default | Description |
|-------|---------|-------------|
| `default_agent` | `"codex"` | Agent used for task execution: `codex`, `claude`, or `anthropic-api` |
| `sandbox_mode` | `"danger-full-access"` | Sandbox policy: `read-only`, `workspace-write`, `danger-full-access` |
| `approval_policy` | `"auto-edit"` | Approval policy for agent actions |

### `[agents.claude]`

| Field | Default | Description |
|-------|---------|-------------|
| `cli_path` | `"claude"` | Path to Claude Code CLI binary |
| `default_model` | `"sonnet"` | Default model for Claude agent |
| `reasoning_budget` | — | Optional reasoning budget for per-phase model selection |

### `[agents.codex]`

| Field | Default | Description |
|-------|---------|-------------|
| `cli_path` | `"codex"` | Path to Codex CLI binary |

### `[agents.codex.cloud]`

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Enable cloud execution mode |
| `cache_ttl_hours` | `12` | Setup phase cache TTL |
| `setup_commands` | `[]` | Commands run during cloud setup phase |
| `setup_secret_env` | `[]` | Env vars available during setup but removed for agent execution |

### `[agents.review]`

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `true` | Enable independent agent review after PR creation |
| `reviewer_agent` | `"codex"` | Agent used for review (must differ from implementor) |
| `max_rounds` | `3` | Maximum review-fix cycles |

### `[review]`

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Enable periodic whole-repo review |
| `interval_hours` | `24` | Hours between review cycles |
| `agent` | — | Agent for review tasks (defaults to `agents.default_agent`) |
| `strategy` | `"single"` | Review mode: `single` (one reviewer) or `cross` (dual-review + synthesis) |
| `timeout_secs` | `900` | Per-turn timeout for review tasks |

When enabled, the scheduler runs a background loop that:

1. Checks for new commits since the last review (`git log --since=<last_review>`)
2. If new commits exist, gathers repo structure, diff stats, and commit log
3. Constructs a comprehensive review prompt and enqueues it as a task
4. The agent reviews the entire codebase and may create a PR with fixes
5. Logs a `periodic_review` event as checkpoint for the next cycle

If no new commits have landed since the last review, the cycle is skipped.

### `[gc]`

| Field | Default | Description |
|-------|---------|-------------|
| `max_drafts_per_run` | `5` | Max remediation drafts generated per GC cycle |
| `budget_per_signal_usd` | `0.50` | Budget cap per signal |
| `total_budget_usd` | `5.0` | Total budget cap per GC run |
| `adopt_wait_secs` | `120` | Wait time before adopting a draft |
| `adopt_max_rounds` | `3` | Max adoption retry rounds |
| `draft_ttl_hours` | `72` | Draft expiration time |

### `[observe]`

| Field | Default | Description |
|-------|---------|-------------|
| `session_renewal_secs` | `1800` | Session renewal interval |
| `log_retention_days` | `90` | Log retention period |

### `[otel]`

| Field | Default | Description |
|-------|---------|-------------|
| `environment` | `"development"` | Environment tag for traces |
| `exporter` | `"disabled"` | OTLP exporter: `disabled`, `otlp-http`, `otlp-grpc` |
| `endpoint` | — | OTLP collector endpoint URL |
| `log_user_prompt` | `false` | Include user prompts in trace spans |

### `[concurrency]`

| Field | Default | Description |
|-------|---------|-------------|
| `max_concurrent_tasks` | `4` | Global maximum concurrent tasks across all projects |
| `max_queue_size` | `32` | Maximum queued tasks before rejecting |

### `[validation]`

| Field | Default | Description |
|-------|---------|-------------|
| `pre_commit` | `[]` | Commands run after agent changes (auto-detected if empty: `cargo fmt`, `cargo check` for Rust) |
| `pre_push` | `[]` | Commands run before pushing |
| `timeout_secs` | `120` | Validation command timeout |
| `max_retries` | `2` | Retry count on validation failure |

### `[[projects]]`

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | yes | — | Unique project identifier |
| `root` | yes | — | Absolute path to project root (must be a git repo) |
| `default` | no | `false` | Mark as default project |
| `default_agent` | no | — | Override `agents.default_agent` for this project |
| `max_concurrent` | no | — | Override `concurrency.max_concurrent_tasks` for this project |

## Task Execution Pipeline

```
1. POST /tasks                    → validate request, resolve project
2. TaskQueue.acquire()            → acquire per-project + global semaphore
3. WorkspaceManager.create()      → create isolated git worktree
4. Agent.execute_stream()         → agent runs in worktree (Claude/Codex/API)
5. PostValidator.run()            → cargo fmt, cargo check (language-detected)
   └─ on failure: retry up to max_retries, agent fixes issues
6. Agent creates PR               → git push + gh pr create
7. Codex review                   → independent review, up to max_rounds
   └─ on issues: agent fixes → Codex re-reviews → repeat
8. QualityGrader.score()          → compute quality grade (A/B/C/D/F)
9. WorkspaceManager.cleanup()     → remove worktree
10. Task status → done/failed
```

## Scheduled Background Systems

Harness runs three background schedulers automatically when the server starts:

### 1. Periodic Review (`[review]`)

Whole-repo code review on a timer. Disabled by default.

```toml
[review]
enabled = true
interval_hours = 24
# strategy = "cross"
```

**What happens when enabled:**
- Every `interval_hours`, checks if new commits exist since the last review
- If yes: gathers repo structure + diff stats + commit log → constructs review prompt → enqueues as a task
- Agent reviews the entire codebase, may create a PR with fixes
- If no new commits: cycle is skipped (no wasted resources)
- Review events are logged to EventStore for audit trail

### 2. Health Tick (always on)

Every 24 hours, runs `RuleEngine::scan()` on the project root:
- Checks all registered guard scripts against the codebase
- Persists violations as `rule_check` events
- Generates a health report with quality grade and violation summary
- Logged as `scheduler: periodic health report`

### 3. GC Runner (always on)

Frequency adapts to code quality:

| Grade | Interval | Meaning |
|-------|----------|---------|
| A (≥90) | 7 days | Code is healthy, rare scans |
| B (≥75) | 3 days | Minor issues, moderate scanning |
| C (≥60) | 1 day | Needs attention, daily scans |
| D (<60) | 1 hour | Critical issues, aggressive scanning |

Scans for violation signals → generates remediation drafts → optionally adopts fixes.

## GC Learn Pipeline (Self-Improving Rules)

Harness can learn from its own execution history: detect recurring problems, generate fixes, and extract reusable rules/skills. This is a 4-step pipeline.

### Prerequisites

- Server running with accumulated task data (`events.db`)
- RPC handshake required before each session:

```bash
curl -X POST http://127.0.0.1:9800/rpc -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize"}'
curl -X POST http://127.0.0.1:9800/rpc -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"initialized"}'
```

### Step 1: Signal Detection (`gc_run`)

Scans the event store for recurring problem patterns:

```bash
curl -X POST http://127.0.0.1:9800/rpc -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"gc_run","params":{"project_id":null}}'
```

Detected signal types:

| Signal | Meaning | Remediation |
|--------|---------|-------------|
| `RepeatedWarn` | Same hook fires N+ warnings | Guard script |
| `ChronicBlock` | M+ hard blocks (CI failures) | Rule |
| `HotFiles` | Same files edited K+ times | Skill |
| `SlowSessions` | Operations exceed T ms | Skill |
| `WarnEscalation` | Warn rate exceeds baseline | Rule |
| `LinterViolations` | M+ violations of same rule | Guard script |

This call spawns an agent per signal to generate remediation drafts. May take several minutes depending on the number of signals and agent availability.

### Step 2: Review Drafts (`gc_drafts`)

List generated drafts and their status:

```bash
curl -X POST http://127.0.0.1:9800/rpc -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":4,"method":"gc_drafts"}'
```

Draft statuses: `pending` → `adopted` | `rejected` | `expired`

You can also inspect drafts directly:

```bash
ls ~/Library/Application\ Support/harness/drafts/
# Each .json file contains: signal, rationale, artifacts (rules/guards/skills)
```

### Step 3: Adopt or Reject (`gc_adopt` / `gc_reject`)

Adopt a draft to mark it as approved for learning:

```bash
# Adopt (also spawns a task to apply the fix)
curl -X POST http://127.0.0.1:9800/rpc -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":5,"method":"gc_adopt","params":{"draft_id":"<DRAFT_ID>"}}'

# Reject
curl -X POST http://127.0.0.1:9800/rpc -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":6,"method":"gc_reject","params":{"draft_id":"<DRAFT_ID>"}}'
```

### Step 4: Extract Rules or Skills (`learn_rules` / `learn_skills`)

After drafts are adopted, extract reusable rules or skills from the remediation content:

```bash
# Extract guard rules from adopted drafts
curl -X POST http://127.0.0.1:9800/rpc -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":7,"method":"learn_rules","params":{"project_root":"/path/to/project"}}'

# Extract reusable skills from adopted drafts
curl -X POST http://127.0.0.1:9800/rpc -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":8,"method":"learn_skills","params":{"project_root":"/path/to/project"}}'
```

These calls invoke an agent to analyze adopted draft artifacts and produce:
- **Rules:** Structured `## RULE_ID: Title` blocks with severity, added to `RuleEngine`
- **Skills:** Structured `=== skill: name ===` blocks, added to `SkillStore`

### Full Pipeline Diagram

```
Events (task execution telemetry)
  ↓
Signal Detector (gc_run)
  ├→ RepeatedWarn
  ├→ ChronicBlock
  ├→ WarnEscalation
  └→ ...
  ↓
Draft Generation (agent analyzes signals)
  ↓
Drafts (pending)
  ↓ (user reviews)
gc_adopt / gc_reject
  ↓
Adopted Drafts
  ↓
learn_rules / learn_skills (agent extracts patterns)
  ↓
RuleEngine / SkillStore (permanently prevents recurrence)
```

### Tips

- **Budget:** Default `budget_per_signal_usd = 0.50` may be too low for complex analysis. Increase to `1.0` in `config/default.toml` if drafts are truncated with "Exceeded USD budget".
- **Timing:** Run `gc_run` after accumulating 50+ tasks for meaningful signals. Running too early produces noise.
- **learn_rules is synchronous:** It blocks until the agent finishes. If other tasks are running, the agent may queue — consider running learn when the server is idle.
- **Manual review:** Always inspect draft content before adopting. Draft quality depends on agent capability and available context.

## CLI Commands

```bash
# Start server
harness serve --transport http --port 9800 --config config/default.toml

# One-shot execution
harness exec "Fix the failing test in src/lib.rs"

# Rule engine
harness rule load .        # Load rules from project
harness rule check .       # Run rule checks

# GC cycle
harness gc run .           # Detect signals, generate remediation drafts

# Skills
harness skill list         # List discovered skills

# ExecPlan
harness plan init spec.md           # Initialize execution plan
harness plan status exec-plan.md    # Check plan status

# Version
harness --version
```

## Troubleshooting

### Server won't start: "sandbox_mode not supported on macOS"

macOS Seatbelt sandbox blocks Claude Code syscalls. Set `sandbox_mode = "danger-full-access"` in config.

### Tasks fail with SIGTRAP

Started server from within Claude Code. Restart from a standalone terminal.

### Codex review shows "unexpected argument"

Codex CLI updated. Check `codex exec --help` for current flags and update `crates/harness-agents/src/codex.rs`.

### All tasks show `no_pr` status

PR extraction failed. Check server logs for agent output. Common cause: agent didn't create a PR (build failure, empty diff).

### Tasks queued but not running

Global concurrency limit reached. Check with:

```bash
curl -s http://127.0.0.1:9800/api/dashboard | python3 -c "import sys,json; d=json.load(sys.stdin); print(f'running={d[\"global\"][\"running\"]} max={d[\"global\"][\"max_concurrent\"]}')"
```

Increase `[concurrency] max_concurrent_tasks` in config if needed.
