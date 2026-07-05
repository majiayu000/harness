# Harness Usage Guide

## Prerequisites

- Rust 1.88+
- Bun 1.1+ for `cargo build --release` when `web/dist` has not already been built
- At least one configured agent runtime, such as Codex CLI, Claude CLI, or the direct Anthropic adapter

## Installation

```bash
git clone https://github.com/majiayu000/harness.git
cd harness
cargo build --release
```

The binary is at `./target/release/harness`.

## Server Startup

`harness serve` can be started directly from a normal terminal. When product
behavior needs live verification from a Codex or Claude agent session, launch
the server with a sanitized environment so spawned agents do not inherit wrapper
variables from the parent process. Harness strips Claude-prefixed variables
before spawning child agents; Codex-prefixed variables are not stripped by the
adapter spawn path, so use `scripts/start-harness-codex-safe.sh` or an
equivalent sanitized launcher when starting from a Codex-owned session. For
long-running manual dogfood sessions, a standalone terminal remains a convenient
way for the operator to own the process lifetime directly.

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
# Set this or HARNESS_API_TOKEN before using HTTP routes.
# api_token = "change-me"
# Intentional tokenless local-dev opt-in:
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

[agents.review]
enabled = true
reviewer_agent = "codex"
max_rounds = 3
required_providers = ["codex_cli_review"]
advisory_providers = ["gemini_github_bot", "codex_github_bot"]
review_bot_auto_trigger = false

[agents.review.codex_cli_review]
enabled = true
base_ref = "origin/main"
output_format = "json"

[gc]
max_drafts_per_run = 5
budget_per_signal_usd = 0.50
total_budget_usd = 5.0
draft_ttl_hours = 72

[observe]
log_retention_max_files = 30
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
# default_agent = "auto" # optional override; or set a registered agent name
```

Start with:

```bash
./target/release/harness --config config/default.toml serve \
  --transport http \
  --port 9800
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
GITHUB_TOKEN=ghp_xxx ./target/release/harness --config config/default.toml serve \
  --transport http \
  --port 9800
```

### With Anthropic API Key

Enable the direct Anthropic API agent:

```bash
ANTHROPIC_API_KEY=sk-ant-xxx ./target/release/harness --config config/default.toml serve \
  --transport http \
  --port 9800
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
{ "status": "queued", "task_id": "a1b2c3d4-..." }
```

The task is accepted immediately and registered as pending work. Use
`GET /tasks/{task_id}` to observe when execution actually starts.

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
  { "task_id": "...", "status": "queued" },
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

The public health payload includes a `runtime_logs` block with the current
logging state, retention window, max-files cap, and a redacted
`logs/<filename>` hint.

## Project Management API

### List Projects

```bash
curl http://127.0.0.1:9800/projects
```

### Register a Project at Runtime

No server restart needed:

```bash
curl -X POST http://127.0.0.1:9800/projects \
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
curl -X DELETE http://127.0.0.1:9800/projects/new-project
```

## Configuration Reference

### `[server]`

| Field | Default | Description |
|-------|---------|-------------|
| `transport` | `"stdio"` | Transport protocol: `stdio`, `http`, or `web_socket` |
| `http_addr` | `"127.0.0.1:9800"` | HTTP listen address |
| `data_dir` | `"~/.local/share/harness"` | Data directory for Harness server state, database metadata, and persisted `harness serve` runtime logs under `logs/` |
| `project_root` | `"."` | Default project root (single-project mode) |
| `api_token` | — | Bearer token for non-exempt HTTP routes. `HARNESS_API_TOKEN` can set the same value during config loading. |
| `allow_unauthenticated` | `false` | Explicit local-dev opt-in for tokenless HTTP operation. Without this or a token, server startup fails closed. Ignored when a token is configured. |
| `github_webhook_secret` | — | HMAC-SHA256 secret for GitHub webhook verification |
| `notification_broadcast_capacity` | `256` | Internal notification channel capacity |

### `[intake.github.auto_merge]`

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Enables server-side auto-merge gating for configured GitHub issue workflows. |
| `method` | `"squash"` | Merge method requested after the deterministic gate passes. |
| `delete_branch` | `true` | Whether merge execution should request source branch cleanup where the selected executor supports it. |
| `merge_execution` | `"agent"` | Selects the merge executor. `"agent"` keeps the current agent-executed merge path and requires server-side completion verification. `"server"` runs the merge through the GitHub REST API, then re-reads the pull request before terminal success. |
| `verify_merge_completion` | `true` | When true, an agent-reported `merge_pr` success is accepted only after Harness re-reads GitHub and observes the PR as merged. |

Server-executed merge mode requires a GitHub token that can merge pull
requests, typically a fine-grained token with repository contents write access
and pull request read access, or an installation token with equivalent
repository permissions. Agent mode does not add token requirements, but merge
completion verification still needs read access to the pull request.

Upgrade note: tokenless HTTP deployments that previously started
unauthenticated now fail at startup. Recover by setting `api_token` /
`HARNESS_API_TOKEN`, or by setting `allow_unauthenticated = true` for a
deliberate local-only deployment.

### `[agents]`

| Field | Default | Description |
|-------|---------|-------------|
| `default_agent` | `"auto"` | Default execution agent; `"auto"` picks the first registered agent |
| `complexity_preferred_agents` | `[]` | Optional ordered list for complex/critical routing (for example `["codex","claude"]`) |
| `sandbox_mode` | `"danger-full-access"` | Sandbox policy: `read-only`, `read-only-with-network`, `workspace-write`, `danger-full-access` |
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
| `enabled` | `true` | Enable independent agent review after PR creation. |
| `reviewer_agent` | `"codex"` | Agent used for review. It may match the implementor when configured explicitly; Harness runs it as a separate review turn. |
| `max_rounds` | `3` | Maximum review-fix cycles |
| `required_providers` | `["codex_cli_review"]` | Local review providers that must approve before local completion can pass. |
| `advisory_providers` | `["gemini_github_bot", "codex_github_bot"]` | Hosted GitHub review bots that are recorded but non-blocking unless external review is explicitly required. |
| `review_bot_auto_trigger` | `false` | Post and wait for a hosted review bot command. When omitted with `enabled = false`, Harness keeps the hosted-bot path; when disabled, local completion requires local review approval, validation, PR-head advancement after local review fixes that changed the workspace or reported a pushed commit, unchanged reviewed PR head after CI polling, green GitHub PR checks, an open PR or already merged PR, and a configured GitHub token. |

### `[agents.review.codex_cli_review]`

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `true` | Enable native local `codex review` execution. |
| `cli_path` | `"codex"` | Path to the Codex CLI binary. |
| `model` | Codex default | Model used for the local review provider. |
| `reasoning_effort` | Codex default | Reasoning effort passed to Codex CLI. |
| `base_ref` | `"origin/main"` | Base ref passed to `codex review --base`. |
| `timeout_secs` | `1800` | Maximum local review runtime. |
| `output_format` | `"json"` | Request a fenced `harness-review-report` block for normalized gating. |

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
| `log_retention_days` | `90` | Runtime log age retention period |
| `log_retention_max_files` | `30` | Maximum matching `harness serve` runtime logs to keep; `0` disables the count cap |

### `[otel]`

| Field | Default | Description |
|-------|---------|-------------|
| `environment` | `"development"` | Environment tag for traces |
| `exporter` | `"disabled"` | OTLP exporter: `disabled`, `otlp-http`, `otlp-grpc` |
| `endpoint` | — | OTLP collector endpoint URL |
| `log_user_prompt` | `false` | Include user prompts in trace spans |
| `trajectory` | `false` | Emit derived workflow, activity, and agent-turn spans when an OTLP exporter is enabled |
| `capture_content` | `false` | Privacy gate for prompt or response content events; keep disabled unless content export has explicit operator approval |

See [OTel Trajectory Quickstart](otel-trajectory-quickstart.md) for a local
Tempo walkthrough.

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

Harness runs several background schedulers automatically when the server starts:

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

### Workflow Runtime Sweepers

Workflow-runtime watchdog and retention sweepers are configured in
`WORKFLOW.md` under `storage`. Both are disabled by default on first rollout:

```yaml
storage:
  orphan_reaper_enabled: true
  orphan_reaper_interval_secs: 3600
  orphan_reaper_legacy_enabled: true
  orphan_reaper_legacy_batch: 200
  workflow_watchdog_enabled: false
  workflow_watchdog_age_minutes: 240
  workflow_watchdog_interval_secs: 300
  workflow_watchdog_batch_size: 100
  runtime_retention_enabled: false
  runtime_retention_days: 30
  runtime_retention_batch_size: 1000
  runtime_retention_interval_secs: 3600
```

The orphan schema reaper is enabled by default. It drops registered
path-derived schemas with dead owner paths and, in bounded batches, legacy
unregistered `h<16-hex>` schemas that cannot be matched to a live workspace
directory or known store path under the configured workspace root.

Enable `workflow_watchdog_enabled` first. It is read-only: aged `blocked` and
`awaiting_feedback` workflow instances appear in `/api/operator-monitor` under
`stuck_workflows` and are logged at error level.

Enable `runtime_retention_enabled` only after the stuck list is clean. Retention
deletes terminal workflow families older than `runtime_retention_days` in
bounded batches and relies on the Postgres runtime-store cascade constraints to
remove events, decisions, commands, jobs, runtime events, and artifacts. Active
workflow families are never pruned.

### 3. GC Runner (always on)

Frequency adapts to code quality:

| Grade | Interval | Meaning |
|-------|----------|---------|
| A (≥90) | 7 days | Code is healthy, rare scans |
| B (≥75) | 3 days | Minor issues, moderate scanning |
| C (≥60) | 1 day | Needs attention, daily scans |
| D (<60) | 1 hour | Critical issues, aggressive scanning |

Scans for violation signals → generates remediation drafts → optionally adopts fixes.

### 4. Self-Evolution Tick (always on)

Every 24 hours, Harness runs a learning cycle for the configured project:
- Calls `learn_rules` to extract reusable guard/rule patterns from adopted drafts
- Calls `learn_skills` to extract reusable execution skills from adopted drafts
- Scores skill outcomes from recent `skill_used` events and task status, then updates
  `governance_status` (`active` / `watch` / `quarantine` / `retired`) with canary gating
- Persists summary events as:
  - `self_evolution_tick` (rules learned / skills learned / skills scored)
  - `skill_governance_tick` (status distribution and transitions)

This is independent of manual GC commands: you can still run `gc_run`, `gc_adopt`, and `learn_*` on demand.

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
- **Auto learning:** Even without manual `learn_*` calls, scheduler ticks will run periodic self-evolution and log results in `self_evolution_tick`.

## CLI Commands

```bash
# Start server
harness --config config/default.toml serve --transport http --port 9800

# One-shot execution
harness exec "Fix the failing test in src/lib.rs"

# Rule engine
harness rule load .        # Load rules from project
harness rule check .       # Run rule checks

# GC cycle
harness gc run .           # Detect signals, generate remediation drafts

# Skills
harness skill list         # List discovered skills

# Container isolation
See `docs/container-tier-operator-guide.md` to build the reference Codex/Claude
agent image, pin it by digest, and route non-collaborator intake to the
container tier.

# ExecPlan
harness plan init spec.md           # Initialize execution plan
harness plan status exec-plan.md    # Check plan status

# PR review
harness pr review 123 --provider codex_cli_review --base origin/main

# Version
harness --version
```

## Troubleshooting

### Server won't start: "sandbox_mode not supported on macOS"

macOS Seatbelt sandbox blocks Claude Code syscalls. Set `sandbox_mode = "danger-full-access"` in config.

### Tasks fail with SIGTRAP

Use a current Harness binary and inspect the server logs for the failing
adapter command. Harness strips Claude-prefixed wrapper variables before
spawning child agents. Codex-prefixed parent variables are not stripped by
Harness, so start the server through a sanitized launcher when it is owned by a
Codex session. A SIGTRAP can also point to a stale binary, adapter configuration,
or macOS sandbox setting.

### Codex review shows "unexpected argument"

Codex CLI updated. Check `codex review --help` for current flags and update `crates/harness-agents/src/codex.rs`.

### All tasks show `no_pr` status

PR extraction failed. Check server logs for agent output. Common cause: agent didn't create a PR (build failure, empty diff).

### Tasks queued but not running

Global concurrency limit reached. Check with:

```bash
curl -s http://127.0.0.1:9800/api/dashboard | python3 -c "import sys,json; d=json.load(sys.stdin); print(f'running={d[\"global\"][\"running\"]} max={d[\"global\"][\"max_concurrent\"]}')"
```

Increase `[concurrency] max_concurrent_tasks` in config if needed.
