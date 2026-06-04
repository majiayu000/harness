# Cost Monitoring Dashboard Spec

## Problem

Harness can run many Codex jobs in parallel through workflow runtime dispatch. Operators currently have to combine runtime tree JSON, process lists, GitHub PR state, and external quota screens to understand why usage is rising. This makes it too easy to confuse "one visible issue workflow" with "one active cost source" while backlog polling, PR feedback sweeps, and sprint planning continue to consume model quota in the background.

The dashboard must make active and historical model usage attributable to concrete workflow work: task, workflow, command, agent invocation, runtime job, repository, activity, model, reasoning effort, and workspace.

## Goals

- Show every currently active Harness-owned agent invocation with repository, subject, activity, model, reasoning effort, age, stable invocation ID, and estimated burn.
- Attribute usage to task-level units such as `repo-backlog:majiayu000/litellm-rs:issue:599`, not only to process IDs.
- Split exact usage from estimated usage. Never present an estimate as provider-confirmed cost.
- Explain quota movement in operator language: active high-effort jobs, low-effort pollers, retry loops, and external user-owned Codex sessions.
- Provide controls to pause, throttle, or cancel expensive workflow categories before the operator exhausts a quota window.
- Support a read-only first release that works without provider billing APIs.

## Non-Goals

- The first release does not need provider-side billing reconciliation.
- The dashboard does not show raw prompts by default.
- The dashboard does not replace workflow state debugging tools.
- The dashboard does not make pricing assumptions that are hardcoded into Rust. Prices and quota weights must come from config.

## Definitions

- **Usage event**: a token, duration, lifecycle, or estimate event associated with a runtime job.
- **Agent invocation**: one Harness-owned runtime job that starts or claims an agent turn. Its stable ID is the runtime job ID until a dedicated invocation table exists.
- **Provider session ID**: a provider or runtime-specific conversation/session identifier. For Codex JSON-RPC this is the thread ID when available.
- **Provider turn ID**: a provider or runtime-specific turn identifier. For Codex JSON-RPC this is the turn ID when available.
- **Session key**: a display key derived from provider IDs when available, such as `<thread_id>-<turn_id>`. It is not the primary Harness attribution key.
- **Exact usage**: token counts or billing values emitted by the model provider, Codex CLI, or Codex JSON-RPC event stream.
- **Estimated usage**: locally derived cost or quota burn from prompt size, elapsed runtime, model, effort, and historical averages.
- **Quota burn units**: normalized units for the operator's current quota window when exact dollar cost is unavailable.
- **Attribution key**: the stable grouping tuple `(project_id, repo, workflow_id, command_id, agent_invocation_id, runtime_job_id, activity, model, reasoning_effort)`.

## Architectural Decisions

1. Harness-owned agents are tracked from workflow runtime state, not from process-name searches.
2. Local process sampling is allowed only for external user-owned Codex or Claude CLI visibility.
3. `runtime_job_id` is the initial `agent_invocation_id` because it is already durable, retry-scoped, and linked to workflow command state.
4. Adapter lifecycle events add process/session/token detail to the invocation record. They do not replace the workflow runtime as the source of attribution.
5. Token counts and costs have separate confidence. Token counts can be exact while dollars remain estimated.
6. Budget controls gate new dispatch only. They must not silently delete or abandon already queued work.

## Design Precedent

Symphony's Elixir orchestrator uses a useful pattern for Codex app-server monitoring:

- the orchestrator owns a `running` map keyed by issue ID
- a running row is created at dispatch time
- the Codex app-server adapter reports OS PID, thread ID, turn ID, session ID, token totals, and rate-limit snapshots back to the orchestrator
- dashboards and APIs render from orchestrator state
- token accounting prefers absolute thread totals such as `thread/tokenUsage/updated.tokenUsage.total`

Harness should adapt the same principle without copying Symphony's exact identifiers. Harness already has better cross-repo attribution through workflow IDs, command IDs, and runtime job IDs. Therefore:

- `agent_invocation_id` stays Harness-owned and stable
- provider session IDs are secondary metadata
- process PID is liveness metadata
- token usage is attached to the invocation through adapter events

## Current Evidence Shape

The current manual investigation requires:

- `GET /api/workflows/runtime/tree` to discover workflow state, command activity, runtime job IDs, model, effort, and stale running rows.
- runtime DB rows to identify Harness-owned agent invocations by stable IDs.
- external local process sampling to identify user-owned Codex or Claude CLI sessions that did not flow through Harness.
- GitHub PR checks to explain why an issue workflow moved from implementation to feedback handling.
- External quota UI to observe the quota percentage.

The dashboard should consolidate these into one operator view and mark stale runtime rows separately from live processes.

## Data Sources

### Runtime Database

Use workflow runtime tables as the durable source for:

- workflow ID, definition, state, subject, parent workflow
- workflow data fields such as `repo`, `project_id`, `task_id`, `issue_number`, `pr_number`, `pr_url`
- command ID, command type, activity, dedupe key
- agent invocation ID, runtime job ID, status, runtime kind, runtime profile, model, reasoning effort, created/updated timestamps
- runtime event count and latest runtime event type

### Agent Event Streams

Parse usage-bearing events from both runtime paths:

- `codex exec --json` stdout events
- Codex JSON-RPC runtime events

The parser must tolerate missing usage fields and store the raw event type metadata without storing raw prompt bodies.

Agent adapters should emit normalized lifecycle events into the workflow runtime event stream:

| Event | Required Fields | Optional Fields | Notes |
|---|---|---|---|
| `agent_invocation_started` | `runtime_job_id`, `command_id`, `workflow_id`, `agent_runtime`, `runtime_profile`, `started_at` | `pid`, `provider_session_id`, `provider_thread_id`, `model`, `reasoning_effort` | Emitted after a runtime job is claimed and the adapter has enough launch metadata. |
| `agent_turn_started` | `agent_invocation_id`, `runtime_job_id`, `started_at` | `provider_thread_id`, `provider_turn_id`, `session_key`, `pid` | Codex JSON-RPC should set thread and turn IDs. Codex exec may omit them. |
| `agent_usage_reported` | `agent_invocation_id`, `runtime_job_id`, `reported_at`, `usage_source`, `usage_confidence` | token fields, `provider_thread_id`, `provider_turn_id`, `raw_event_kind` | Raw prompt text must not be stored. |
| `agent_rate_limits_reported` | `agent_invocation_id`, `runtime_job_id`, `reported_at` | redacted rate-limit payload | Optional, displayed only when available. |
| `agent_invocation_heartbeat` | `agent_invocation_id`, `runtime_job_id`, `reported_at` | `pid`, `last_event_kind`, `lease_owner` | Keeps stale detection independent from process scanning. |
| `agent_invocation_finished` | `agent_invocation_id`, `runtime_job_id`, `ended_at`, `terminal_status` | final token fields, failure summary | Closes the invocation and updates rollups. |

These events are the target contract. The first read-only release may derive active invocations from existing `runtime_jobs`, but the API shape should already separate Harness-owned invocations from external local processes.

### External Process Sampler

Sample active local Codex and Claude CLI processes that are not already attributed through workflow runtime state:

- PID, elapsed time, CPU, memory
- agent type and a coarse command label, such as `codex exec` or `claude`
- no raw argv, prompt text, environment, or workspace path

This sampler is an external-session liveness hint, not the source of truth for Harness-owned agents and not an authoritative billing source. Harness-owned agents must be represented by agent invocation records from the runtime database and runtime events.

Process sampling must exclude:

- desktop app helper processes such as Claude Helper
- browser native-host helper processes
- shell wrapper or waiter processes that merely contain `codex` or `claude` in a string

If a process can be matched to a Harness invocation through a recorded PID, runtime job id, or command id, it belongs under `agent_invocations` and must not be duplicated under `external_agent_processes`.

### Provider or Plan Quota

If Codex exposes a local or remote quota API, ingest it through a `QuotaProvider` trait. If not, allow manual snapshots:

- observed quota percent
- observed timestamp
- quota window length
- optional operator note

Manual snapshots support calibration of quota burn units without pretending to know exact provider accounting.

## Data Model

### `usage_events`

Stores raw normalized usage events.

| Field | Type | Notes |
|---|---|---|
| `id` | UUID | Primary key |
| `agent_invocation_id` | UUID | Nullable for external sessions until a dedicated invocation table exists |
| `runtime_job_id` | UUID | Nullable for external Codex sessions |
| `workflow_id` | Text | Nullable for external sessions |
| `command_id` | UUID | Nullable |
| `project_id` | Text | Nullable |
| `repo` | Text | Nullable |
| `subject_key` | Text | Nullable |
| `activity` | Text | Nullable |
| `runtime_kind` | Text | `codex_exec`, `codex_jsonrpc`, etc. |
| `runtime_profile` | Text | Profile name |
| `model` | Text | Model name from runtime profile or event |
| `reasoning_effort` | Text | `low`, `medium`, `high`, `xhigh`, nullable |
| `event_kind` | Text | `token_usage`, `estimate_tick`, `process_sample`, `quota_snapshot` |
| `source` | Text | `codex_exec_json`, `codex_jsonrpc`, `process_sampler`, `manual_quota`, `provider_quota` |
| `confidence` | Text | `exact`, `estimated`, `observed`, `unknown` |
| `prompt_tokens` | BigInt | Nullable |
| `completion_tokens` | BigInt | Nullable |
| `cached_input_tokens` | BigInt | Nullable |
| `reasoning_tokens` | BigInt | Nullable |
| `wall_time_ms` | BigInt | Nullable |
| `cost_usd_estimated` | Decimal | Nullable |
| `quota_units_estimated` | Decimal | Nullable |
| `occurred_at` | Timestamp | Event timestamp |
| `metadata` | JSONB | Redacted metadata only |

### `agent_invocations`

Records Harness-owned agent starts and claims.

| Field | Type | Notes |
|---|---|---|
| `agent_invocation_id` | UUID | Primary key; initially equal to `runtime_job_id` |
| `runtime_job_id` | UUID | Workflow runtime job that caused the invocation |
| `workflow_id` | Text | Required |
| `command_id` | UUID | Required |
| `agent_runtime` | Text | `codex_exec`, `codex_jsonrpc`, `claude_code`, etc. |
| `runtime_profile` | Text | Profile name |
| `model` | Text | Model requested for this invocation |
| `reasoning_effort` | Text | Reasoning effort requested for this invocation |
| `lease_owner` | Text | Runtime worker/host that claimed the job |
| `pid` | Integer | Nullable until adapters emit spawn lifecycle events |
| `provider_session_id` | Text | Nullable; provider/runtime conversation or session ID |
| `provider_thread_id` | Text | Nullable; Codex JSON-RPC thread ID when available |
| `provider_turn_id` | Text | Nullable; current or latest turn ID when available |
| `session_key` | Text | Nullable; display key such as `<thread_id>-<turn_id>` |
| `worker_host` | Text | Runtime host that executed the invocation, nullable for local execution |
| `workspace_path` | Text | Redacted/local display path, nullable |
| `last_event_kind` | Text | Last normalized adapter/runtime event |
| `last_usage_reported_at` | Timestamp | Last token event timestamp |
| `usage_confidence` | Text | `exact`, `estimated`, `observed`, or `unknown` |
| `started_at` | Timestamp | Invocation start |
| `last_seen_at` | Timestamp | Last runtime or usage event |
| `ended_at` | Timestamp | Completion time |

### `runtime_job_usage`

Materialized or maintained rollup by runtime job.

| Field | Type | Notes |
|---|---|---|
| `runtime_job_id` | UUID | Primary key |
| `agent_invocation_id` | UUID | Agent invocation that produced the usage |
| `workflow_id` | Text | Required when known |
| `command_id` | UUID | Required when known |
| `task_id` | Text | From workflow data |
| `repo` | Text | Repository slug |
| `activity` | Text | Runtime activity |
| `status` | Text | Latest runtime job status |
| `model` | Text | Latest model |
| `reasoning_effort` | Text | Latest effort |
| `started_at` | Timestamp | Runtime job creation |
| `last_seen_at` | Timestamp | Last usage or process sample |
| `ended_at` | Timestamp | Completion time |
| `prompt_tokens` | BigInt | Exact when available |
| `completion_tokens` | BigInt | Exact when available |
| `reasoning_tokens` | BigInt | Exact when available |
| `total_cost_usd_estimated` | Decimal | Exact price calculation or estimate |
| `total_quota_units_estimated` | Decimal | Normalized quota burn |
| `usage_confidence` | Text | Lowest-confidence contributor wins |
| `provider_session_id` | Text | Nullable |
| `provider_turn_id` | Text | Nullable |
| `live_pid` | Integer | Nullable |
| `stale_running` | Boolean | Runtime says running but process is gone or heartbeat is stale |

### `usage_price_catalog`

Configurable model and effort weights.

| Field | Type | Notes |
|---|---|---|
| `model` | Text | Model name |
| `effective_from` | Timestamp | Price version start |
| `input_usd_per_million_tokens` | Decimal | Nullable |
| `output_usd_per_million_tokens` | Decimal | Nullable |
| `cached_input_usd_per_million_tokens` | Decimal | Nullable |
| `reasoning_usd_per_million_tokens` | Decimal | Nullable |
| `quota_weight` | Decimal | Relative quota burn multiplier |
| `source_note` | Text | Where the operator entered or verified this price |

### `usage_budget_policies`

Stores enforcement and alert policies.

| Field | Type | Notes |
|---|---|---|
| `id` | UUID | Primary key |
| `scope_type` | Text | `server`, `repo`, `activity`, `workflow`, `model`, `profile` |
| `scope_value` | Text | Scope identifier |
| `window_secs` | Integer | Rolling window |
| `max_quota_units` | Decimal | Nullable |
| `max_cost_usd` | Decimal | Nullable |
| `max_concurrency` | Integer | Nullable |
| `action` | Text | `alert`, `throttle`, `block_new`, `require_approval`, `cancel_low_priority` |
| `enabled` | Boolean | Policy switch |

## Token Accounting Contract

### Codex JSON-RPC / App-Server

Codex JSON-RPC should follow absolute-total accounting:

1. Prefer `thread/tokenUsage/updated.tokenUsage.total` when available.
2. Fall back to token-count events containing `total_token_usage`.
3. Track the last accepted absolute total per `(agent_invocation_id, provider_thread_id)`.
4. Add only the positive delta between the new absolute total and the last accepted total.
5. Ignore `tokenUsage.last` and `last_token_usage` for dashboard totals unless no absolute total source exists and the event contract explicitly marks it as additive.
6. Do not reset accounting when a new turn starts on the same provider thread.

The UI may display the latest provider thread/turn/session key, but all rollups group by `agent_invocation_id` and workflow attribution fields.

### Codex Exec JSON

`codex exec --json` should be parsed as one or more event streams:

- lifecycle events update invocation status and last activity
- usage events update exact token counters when the JSON payload has a documented usage schema
- completion events close the invocation and mark final status

If a `codex exec` payload does not include exact token usage, Harness must keep token fields null and use estimated burn only. It must not infer exact token counts from elapsed time.

### Claude Code and Other Runtimes

Runtimes that do not emit token usage use lifecycle and heartbeat events for live cost pressure only:

- active status
- runtime profile
- model and effort if configured
- elapsed time
- historical median burn by activity and model

Rows must show `usage_confidence=estimated` until exact provider or runtime usage arrives.

## Cost and Quota Calculation

### Exact Token Path

When token counts are available:

1. Store the raw token fields as exact usage events.
2. Join to `usage_price_catalog` by model and timestamp.
3. Calculate estimated USD from configured prices.
4. Calculate quota units from configured quota weights.
5. Mark confidence as `exact` for tokens and `estimated` for dollars unless provider cost is confirmed.

### Estimated Path

When exact tokens are unavailable:

1. Estimate prompt tokens from prompt packet byte size.
2. Estimate output tokens from rolling historical medians by `(activity, model, reasoning_effort)`.
3. Add elapsed-time burn for running jobs with no final token data.
4. Apply model and reasoning multipliers from config.
5. Mark confidence as `estimated`.

### Quota Calibration

Manual quota snapshots can calibrate burn units:

1. Operator records quota percentage at `t1`.
2. Operator records quota percentage at `t2`.
3. Harness compares observed quota delta with estimated job burn in the same window.
4. The dashboard shows a calibration factor and confidence range.

This allows useful "why did usage jump" debugging even when exact provider quota APIs are unavailable.

## API Design

### First Read-Only Release

`GET /api/usage-monitor` ships the initial dashboard payload as one aggregated read-only endpoint. It combines:

- exact token usage already stored in `llm_usage`
- active and recent Harness-owned agent invocations from the workflow database
- local Codex and Claude process samples only for external-session visibility
- optional estimated USD values when `HARNESS_USAGE_PRICE_CATALOG_JSON` is configured

The endpoint must not hardcode model prices. If no price catalog is configured, token counts remain exact and cost fields remain null with diagnostics explaining why.

Current response fields:

| Field | Meaning |
|---|---|
| `window` | Requested lookback window and server timestamp |
| `cost` | Price catalog configuration state and cost-confidence message |
| `summary` | Token totals, request count, active invocation counts, stale counts, high-burn counts |
| `tokens_by_agent` | Historical exact token usage grouped by usage event agent |
| `tokens_by_project` | Historical exact token usage grouped by project field |
| `tokens_by_model` | Historical exact token usage grouped by model |
| `agent_invocations` | Harness-owned runtime jobs derived from workflow runtime state |
| `external_agent_processes` | Local Codex/Claude CLI process hints outside Harness attribution |
| `active_by_repo` | Active Harness invocation pressure grouped by repo |
| `active_by_activity` | Active Harness invocation pressure grouped by activity |
| `diagnostics` | Runtime store availability and source labels |

The first release may show `estimated_runtime_burn` for active invocation rows because exact per-invocation tokens are not yet wired from adapters. This is acceptable only if the row also keeps token/cost confidence labels explicit.

The split endpoints below are the target API shape for follow-up releases once persistence, budget controls, and historical drill-downs mature.

### `GET /api/usage/summary`

Returns top-level dashboard numbers.

Response fields:

- active model jobs
- active high-effort jobs
- estimated quota burn per hour
- total estimated quota burn in current window
- stale running rows
- top repositories by current-window burn
- top activities by current-window burn
- latest quota snapshot when available

### `GET /api/usage/running`

Returns active and stale-running jobs.

Each row includes:

- runtime job ID
- PID and process age when alive
- repo, subject, workflow state, activity
- model, reasoning effort, runtime kind, profile
- exact tokens if available
- estimated current burn
- usage confidence
- links to workflow detail, workspace, issue, and PR

### `GET /api/usage/jobs/{runtime_job_id}`

Returns one runtime job usage timeline:

- lifecycle events
- token usage events
- process samples
- prompt packet digest
- related workflow commands
- related GitHub issue or PR metadata
- redaction-safe metadata

### `GET /api/usage/breakdown`

Query params:

- `group_by=repo|activity|model|reasoning_effort|task_id|workflow_id`
- `window=5h|24h|7d|custom`
- `confidence=all|exact|estimated`

### `POST /api/usage/quota-snapshots`

Records an operator-observed quota percentage when provider APIs are unavailable.

### `GET /api/usage/export.csv`

Exports rows for external analysis.

## Dashboard UX

### Top Bar

Show the most operationally important numbers:

- quota window remaining or latest observed quota percent
- active burn rate
- active jobs
- high-effort jobs
- stale running rows
- estimated time to threshold

Each number must show a confidence badge: `Exact`, `Estimated`, or `Observed`.

### Running Jobs Table

Default sort: highest estimated burn first, then highest reasoning effort, then oldest runtime.

Columns:

- status: live, stale, completing, blocked
- repo
- subject
- activity
- model
- effort
- age
- estimated current-window burn
- confidence
- PID
- PR or issue
- actions

Actions:

- view details
- pause workflow
- cancel runtime job
- throttle repo
- copy diagnostic command

Destructive actions require confirmation and must write an audit event.

### Cost Breakdown

Provide grouped views:

- by repository
- by task
- by activity
- by model and reasoning effort
- by runtime profile

The task grouping is the primary view for "which task is spending quota?"

### Job Detail Drawer

Show:

- workflow and command identifiers
- prompt packet digest and source path
- runtime profile
- live process sample history
- token timeline
- related GitHub PR checks
- validation or failure summary
- exact fields used for cost attribution

Do not display raw prompts unless the operator explicitly expands a redacted debug section.

### Budget Policy View

Operators can define policies such as:

- at most 1 concurrent `xhigh` job
- pause `poll_repo_backlog` when the quota window is above 25% consumed
- require approval before `plan_repo_sprint` starts on `gpt-5.5` with `xhigh`
- cap `repo_backlog` to 1 concurrent job
- disable low-priority polling while any issue implementation is active

## Control Plane

### Throttling

Introduce a `UsageGate` before runtime job dispatch:

1. Load active budget policies.
2. Calculate current usage and active concurrency.
3. Decide `allow`, `delay`, `require_approval`, or `deny`.
4. Record the decision as a workflow event.

The gate must never silently drop work. Delayed work remains queued with an explicit reason.

### Priority Classes

Recommended default priority:

1. `address_pr_feedback` for existing open PRs
2. `implement_issue`
3. `quality_gate`
4. `plan_repo_sprint`
5. `poll_repo_backlog`

When quota is tight, pause lower classes first.

### Stale Job Handling

A runtime job is `stale_running` when:

- status is `running`
- latest runtime event is older than lease TTL
- no live process is found
- no remote runtime host heartbeat is associated with the job

Stale rows should be excluded from active burn rate, but shown as reliability issues.

## Implementation Plan

### Phase 0: Read-Only Visibility

- Add read-only usage endpoints backed by workflow runtime state.
- Represent Harness-owned runtime jobs as `agent_invocations`.
- Add process sampler only for external Codex/Claude CLI visibility.
- Show active invocations, stale rows, high-burn rows, historical token groups, and estimated burn.
- Add optional local price catalog parsing through `HARNESS_USAGE_PRICE_CATALOG_JSON`.
- No enforcement.

### Phase 1: Usage Persistence

- Add `usage_events`, `agent_invocations`, `runtime_job_usage`, and `usage_price_catalog`.
- Emit adapter lifecycle events for invocation start, turn start, heartbeat, usage, rate-limit snapshots, and finish.
- Parse usage events from `codex exec --json` when exact usage fields exist.
- Parse absolute token totals from Codex JSON-RPC.
- Reconcile usage on runtime job completion.
- Add CSV export.

### Phase 2: Dashboard UI

- Add top-level usage route in the existing web UI.
- Add running job table, grouped breakdown, and job detail drawer.
- Add confidence labels everywhere cost or quota is displayed.
- Link usage rows back to workflow, workspace, PR, and issue.

### Phase 3: Budget Enforcement

- Add `usage_budget_policies`.
- Add `UsageGate` before runtime dispatch.
- Add throttle and approval decisions for new runtime jobs.
- Add audited cancel, pause, and resume actions.
- Keep queued work visible when policy delays dispatch.

### Phase 4: Provider Integration

- Add optional `QuotaProvider` implementations if Codex or provider APIs expose reliable quota or billing data.
- Keep provider integration optional so local read-only visibility still works offline.

## Testing

### Unit Tests

- Parse token usage from known `codex exec --json` fixtures.
- Parse token usage from known JSON-RPC fixtures.
- Calculate USD estimates from price catalog rows.
- Calculate quota units from model and reasoning weights.
- Mark stale-running jobs correctly.
- Redact metadata before persistence.

### Integration Tests

- Create fake runtime jobs and process samples without launching real Codex.
- Verify `/api/usage-monitor` returns exact usage, active runtime jobs, process samples, and cost diagnostics.
- Verify Harness-owned invocation counts come from runtime jobs even when no local process is sampled.
- Verify external process rows do not include desktop helpers, browser native hosts, or shell waiters.
- Verify `/api/usage/running` groups runtime database rows with live process samples.
- Verify stale rows are not counted in active burn.
- Verify budget policies delay or block only new dispatches.

### UI Tests

- Render active jobs table with exact and estimated rows.
- Render empty state with no active model jobs.
- Render stale-running warning.
- Verify destructive actions require confirmation.

## Security and Privacy

- Do not persist raw prompts in usage tables.
- Redact command metadata that may include secrets.
- Treat workspace paths as local operational metadata and avoid exporting them unless explicitly requested.
- Require an operator role for cancel, pause, resume, and budget policy changes.
- Audit every control-plane action with actor, timestamp, target, and reason.

## Acceptance Criteria

- An operator can answer "what is burning quota right now?" in one screen.
- Active jobs are grouped by task, repo, activity, model, and reasoning effort.
- Stale runtime rows are visible but excluded from active burn.
- Every cost number is labeled exact, estimated, or observed.
- The dashboard can operate without provider billing APIs.
- Harness-owned agent counts do not depend on command-line process matching.
- External Codex/Claude processes are shown separately from Harness invocations.
- Budget gates can prevent more than one concurrent `xhigh` job.
- Low-priority backlog polling can be paused automatically when quota is tight.

## Open Questions

- Does the local Codex runtime expose exact token usage for every `codex exec --json` turn?
- Is there a reliable local or remote source for the operator's five-hour quota percentage?
- Should budget thresholds default to hard blocking or approval-required mode?
- Should repo backlog polling be rewritten to use deterministic GitHub queries before invoking any model?
- Can external user-owned Codex sessions expose enough structured usage to be imported without confusing them with Harness-owned invocations?
