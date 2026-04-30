# Workflow Runtime Decoupling Plan

## Goal

Split Harness into two explicit layers:

- workflow orchestration: state, events, decisions, commands, artifacts, leases, and recovery
- runtime execution: Codex, Claude Code, Anthropic API, or remote workers that execute accepted jobs

The workflow layer decides what should happen through agent-produced structured decisions.
The runtime layer only executes approved jobs and returns structured activity results.

## Current Implementation Status

This branch is an isolated phase-2 worktree based on `origin/main`.

Implemented now:

- generic workflow/runtime contract types in `harness-workflow::runtime`
- Postgres-backed `WorkflowRuntimeStore`
- transition validator and deterministic plan-issue policy adapter
- optional server startup wiring for `{workflow_namespace}_runtime`
- `PLAN_ISSUE` durable event/decision/command recording
- `ReplanCompleted` event recording after a successful replan
- PR detection durable binding through `PrDetected` / `bind_pr`
- PR feedback decisions for waiting, addressing feedback, and ready-to-merge
- repo backlog decisions for issue discovery, merged PR reconciliation, and stale workflow recovery
- generic runtime worker boundary for claiming jobs and recording structured activity results
- workflow-first runtime tree API and dashboard panel for instances, commands, runtime jobs, and
  rejected decisions
- workflow command outbox dispatcher that converts pending runtime commands into runtime jobs
- opt-in server background loop for workflow command dispatch
- opt-in server background runtime worker that executes runtime jobs through registered agents
- runtime job completion writes back command status and workflow completion events
- runtime completion reducer advances known workflow states from activity output
- runtime completion reducer returns repo backlog workflows to `idle` after successful dispatch or
  reconciliation activities
- runtime completion reducer can retry failed activities when the workflow instance declares a
  bounded retry policy
- `WORKFLOW.md` can define a global failed activity retry budget and activity-specific retry
  overrides that are copied into workflow runtime instance data
- runtime command dispatch can select runtime profiles per workflow/activity pair, then activity,
  then workflow
- runtime profiles carry model and execution metadata into runtime jobs
- server runtime workers pass runtime profile model overrides into agent turns
- server turn lifecycle enforces runtime profile `timeout_secs` as an execution deadline
- server runtime workers pass profile reasoning effort and sandbox overrides into agent requests
- workflow runtime workers enforce profile `max_turns` as a workflow-instance runtime turn budget
- server runtime workers pass Codex runtime profile `approval_policy` into Codex exec and
  app-server requests

Still intentionally not moved yet:

- existing task runner ownership of process execution
- repo backlog polling as the primary controller
- workflow-first replacement for legacy task submission routes
- dashboard write actions still use existing task routes

## Non-Goals

- Do not replace the current task execution path in the first change.
- Do not remove existing Codex, Claude, workspace, queue, dashboard, or recovery code.
- Do not introduce a YAML workflow DSL before the runtime contract is proven.
- Do not make Rust code interpret GitHub PR feedback as product policy.

## Target Architecture

```text
Source adapters
  -> WorkflowEvent
  -> WorkflowController
  -> WorkflowPolicyAgent
  -> WorkflowDecision
  -> TransitionValidator
  -> WorkflowCommand
  -> RuntimeJob
  -> RuntimeWorker
  -> RuntimeEvent
  -> ActivityResult
  -> WorkflowEvent
```

## Layer Responsibilities

### Workflow Definition

Owns:

- state names and allowed transitions
- source event vocabulary
- activity names
- prompt policy
- structured decision schema
- retry, replan, feedback, and quality-gate policy
- acceptance and validation requirements

Does not own:

- Codex or Claude CLI argument construction
- runtime process management
- direct GitHub or git subprocess execution

### Workflow Runtime

Owns:

- workflow instances
- workflow events
- accepted and rejected decisions
- command outbox
- artifacts
- workflow leases
- idempotency keys
- transition validation
- resource budget checks

Does not own:

- product judgment about PR feedback
- deciding whether a plan concern is valid
- interpreting CI output beyond structured runtime result schemas

### Agent Runtime

Owns:

- claiming runtime jobs
- executing Codex, Claude Code, Anthropic API, or a remote worker
- streaming runtime events
- writing activity results
- surfacing runtime errors

Does not own:

- mutating workflow state directly
- deciding parent workflow transitions outside its structured output

## Initial Workflow Set

### `repo_backlog`

Purpose:

- discover issues and PRs for a repo
- reconcile GitHub state with local workflow state
- start or wake issue workflows

Main outputs:

- `IssueDiscovered`
- `IssueUpdated`
- `PrUpdated`
- `PrReviewUpdated`
- `RecoveryRequested`

### `github_issue_pr`

Purpose:

- own a single issue lifecycle
- bind PRs to issues
- coordinate implementation, replan, feedback, and readiness

Main states:

- `discovered`
- `scheduled`
- `planning`
- `implementing`
- `replanning`
- `pr_open`
- `awaiting_feedback`
- `addressing_feedback`
- `ready_to_merge`
- `blocked`
- `done`
- `failed`
- `cancelled`

### `pr_feedback`

Purpose:

- inspect PR comments, review state, checks, and mergeability
- produce a structured feedback report

Main outputs:

- `FeedbackFound`
- `NoFeedbackFound`
- `ChangesRequested`
- `Approved`
- `ChecksFailed`
- `ChecksPassed`
- `Mergeable`

### `quality_gate`

Purpose:

- verify configured validation commands
- convert runtime validation into structured artifacts

Main outputs:

- `QualityPassed`
- `QualityFailed`
- `QualityBlocked`

## Data Model

The durable bus uses Postgres tables:

- `workflow_definitions`
- `workflow_instances`
- `workflow_events`
- `workflow_decisions`
- `workflow_commands`
- `runtime_jobs`
- `runtime_events`
- `workflow_artifacts`

The current implementation creates these tables in the optional
`{workflow_namespace}_runtime` schema. If the store cannot initialize, existing task execution
continues and the server reports the optional subsystem as degraded.

## Runtime Profiles

Runtime jobs reference profiles by name.
Runtime-specific details stay outside workflow definitions.

Initial runtime kinds:

- `codex_exec`
- `codex_jsonrpc`
- `claude_code`
- `anthropic_api`
- `remote_host`

Runtime profile fields:

- runtime kind
- model
- reasoning effort
- sandbox
- approval policy
- max turns
- timeout
- environment policy
- working directory
- tool allowlist

## Prompt Packet Contract

Every runtime job receives a complete prompt packet:

- workflow definition name and version
- workflow instance id and current state
- triggering source event
- allowed decisions or activity result schema
- project root and repository identity
- subject key, such as `issue:123` or `pr:456`
- recent workflow event history
- parent and child workflow links
- previous artifacts
- resource budget summary
- hard constraints
- acceptance criteria
- validation requirements
- required structured output schema

The runtime should persist a prompt digest or redacted prompt packet for audit and recovery.

## Migration Plan

### Phase 0: Core Contract

Status: implemented in `harness_workflow::runtime`.

Added a new module under `harness-workflow` with:

- workflow definitions
- workflow instances
- workflow events
- workflow decisions
- workflow commands
- runtime jobs
- runtime events
- activity results
- transition validator

Tests:

- serde round trips for every public contract type
- valid replan decision is accepted
- invalid state mismatch is rejected
- unallowed command is rejected
- duplicate dedupe key is rejected
- terminal reopen is rejected by default
- runtime job and activity result round trips are stable

### Phase 1: Shadow Bus

Status: partially superseded by phase 2.

Keep existing task-first execution.
Write workflow events and decisions in parallel while the existing task runner remains the
execution path.

Tests:

- existing task creation still works
- issue task completion writes workflow events
- PR URL detection writes a PR binding artifact

### Phase 2: Replan Through Workflow Decision

Status: implemented for `PLAN_ISSUE`.

Move `PLAN_ISSUE` handling out of executor-local failure handling.

Flow:

```text
implementation runtime job emits PlanIssueRaised
  -> workflow policy decision
  -> validator accepts RunReplan
  -> command outbox records replan_issue
  -> existing executor runs replan during this transition period
  -> ReplanCompleted event
  -> implementation runtime job resumes with revised plan
```

Tests:

- `PLAN_ISSUE` does not directly mark the issue workflow failed
- force-execute records the concern and continues
- repeated replan hits the configured loop limit and blocks

### Phase 3: PR Feedback Workflow

Status: partially implemented.

The current branch records PR feedback decisions into the workflow runtime while keeping the
existing review loop as the execution path.

Implemented now:

- `PrDetected` events bind issue workflows to PRs through a validated `bind_pr` decision
- review waits record `NoFeedbackFound` / `wait_for_pr_feedback`
- actionable review or validation failures record `FeedbackFound` / `address_pr_feedback`
- approved reviews and graduated low-risk exits record `PrReadyToMerge` / `mark_ready_to_merge`

Still intentionally not moved yet:

- GitHub signal interpretation still runs in the existing review loop
- feedback fix execution still runs through task runner turns
- `pr_feedback` is not yet a separate child workflow with its own runtime job

Tests:

- feedback report with blocking items moves parent workflow to `addressing_feedback`
- no actionable feedback keeps parent workflow in `awaiting_feedback`
- approved and checks-passed events can move parent workflow to `ready_to_merge`

### Phase 4: Repo Backlog Workflow

Status: partially implemented.

The current branch records repo backlog decisions into the workflow runtime while keeping the
existing intake poller, sprint planner, and reconciliation loop as the execution path.

Implemented now:

- open GitHub issues without an existing issue workflow record `IssueDiscovered` and emit a
  validated `start_issue_workflow` child workflow command
- externally merged PRs record `PrMerged`, emit `mark_bound_issue_done`, and update the bound
  issue workflow to `done`
- startup recovery of stale active issue workflows records `RecoveryRequested` and emits
  `recover_issue_workflow`

Still intentionally not moved yet:

- GitHub polling still runs through the existing intake source implementations
- sprint planning still uses the current task queue path
- command outbox rows are not yet claimed by runtime workers

Tests:

- open issue without workflow emits start command
- merged PR updates the bound issue workflow
- stale active workflow emits recovery event

### Phase 5: Runtime Worker Abstraction

Status: partially implemented.

The current branch adds the generic worker boundary while leaving the existing Codex and Claude task
execution paths in place.

Implemented now:

- `RuntimeJobExecutor` trait for runtime-specific execution adapters
- `RuntimeWorker` that claims one pending runtime job, records claim/result events, executes the
  adapter, and completes the durable job
- `ActivityResult::cancelled` so cancellation can be represented as a structured runtime result

Still intentionally not moved yet:

- Codex exec/jsonrpc and Claude Code adapters still run through the current task executor
- command outbox rows are not yet automatically converted into runtime jobs
- runtime workers are not yet spawned as server-owned background processes

Tests:

- runtime worker claims one job once
- runtime events stream in sequence
- failed runtime job records structured error
- cancelled runtime job releases lease

### Phase 6: Workflow-First UI

Status: partially implemented.

Keep task drill-down, but make workflow tree the primary dashboard surface.

Implemented now:

- `GET /api/workflows/runtime/tree` returns workflow instances as a parent/child tree
- each node includes events, decisions, command outbox rows, and runtime jobs
- the dashboard active view shows a compact workflow runtime panel above the task kanban
- rejected decisions display operator-readable reasons

Still intentionally not moved yet:

- task cards and merge actions still use existing task endpoints
- workflow runtime tree is read-only
- command outbox rows are not yet the primary dispatch source for runtime jobs

Tests:

- workflow tree displays parent and child workflows
- runtime jobs are visible under workflow activities
- rejected decisions show operator-readable reasons

### Phase 7: Command Outbox Dispatch

Status: partially implemented.

Convert accepted workflow commands into durable runtime jobs without making the existing task runner
depend on the new path yet.

Implemented now:

- `RuntimeCommandDispatcher` scans pending command outbox rows
- commands that require runtime execution enqueue `runtime_jobs`
- non-runtime commands are marked `skipped` so they do not stay pending forever
- repeated dispatch is idempotent when a runtime job already exists for the command

Still intentionally not moved yet:

- the server does not spawn the dispatcher in a background loop
- runtime profile selection is a single default profile, not workflow-specific policy
- command lease/claiming is not yet a multi-dispatcher concurrency protocol

Tests:

- activity commands enqueue runtime jobs with prompt input context
- non-runtime commands are skipped without creating jobs

### Phase 8: Server Dispatch Loop

Status: partially implemented.

Run the command outbox dispatcher from the server as an opt-in background loop.

Implemented now:

- `WORKFLOW.md` supports `runtime_dispatch` policy fields
- the server spawns a weak-reference runtime command dispatcher loop when the workflow runtime store
  is available
- the loop is disabled by default while workflow runtime execution remains opt-in
- each tick converts pending command rows into runtime jobs using the configured runtime profile

Still intentionally not moved yet:

- no server-owned runtime worker loop consumes the jobs
- runtime profile selection is not yet workflow-specific
- command claiming is still single-dispatcher safe but not a distributed lease protocol

Tests:

- workflow config parses dispatch policy fields
- server dispatch tick creates runtime jobs and marks commands dispatched

### Phase 9: Server Runtime Worker Loop

Status: partially implemented.

Run claimed runtime jobs through the existing agent turn lifecycle while keeping the workflow
runtime opt-in.

Implemented now:

- `WORKFLOW.md` supports `runtime_worker` policy fields
- the server can claim pending runtime jobs on a configurable worker tick
- runtime jobs create normal Harness threads and turns, so agent output remains visible in the
  existing conversation/runtime log path
- completed turns are written back as structured `ActivityResult` payloads
- `remote_host` jobs remain reserved for external runtime hosts and are not executed locally

Still intentionally not moved yet:

- non-Codex approval policy remains metadata until those runtimes define compatible contracts
- workflow completion reducer coverage is intentionally limited to known activity/state pairs
- runtime workers are process-local; distributed worker leasing beyond the job claim is not added yet

Tests:

- workflow config parses worker policy fields
- server worker tick claims a job, runs a registered agent, and records a succeeded runtime job

### Phase 10: Runtime Completion Feedback

Status: partially implemented.

Feed runtime job completion back into the workflow event stream.

Implemented now:

- runtime workers update the originating command status to `completed`, `failed`, or `cancelled`
- runtime workers append `RuntimeJobCompleted` events to the parent workflow when the command row
  is known, including the original workflow command payload for retry/reducer policy
- direct runtime jobs without a workflow command row still complete normally
- workflow runtime instance data carries configured `runtime_retry_policy` values from
  `WORKFLOW.md` when policy is present

Still intentionally not moved yet:

- reducer coverage is limited to known activity/state pairs
- activity result schemas are not yet workflow-specific
- retry backoff and retry cooldown windows are not yet modeled

Tests:

- runtime worker completion updates command status and appends a workflow event
- existing runtime worker tests still cover direct jobs without command rows

### Phase 11: Runtime Completion Reducer

Status: partially implemented.

Consume `RuntimeJobCompleted` events and advance workflow state for known activity results.

Implemented now:

- `replan_issue` completion moves GitHub issue workflows from `replanning` to `implementing`
- `address_pr_feedback` completion moves GitHub issue workflows from `addressing_feedback` to
  `awaiting_feedback`
- successful repo backlog dispatch and reconciliation activity completion moves `dispatching` and
  `reconciling` backlog workflows back to `idle`
- failed, blocked, and cancelled activity results transition workflows to `failed`, `blocked`, or
  `cancelled` through validated workflow decisions
- failed activity results retry the same activity before failing when
  `runtime_retry_policy.max_failed_activity_retries` is present on the workflow instance data
- activity-specific retry budgets under `runtime_retry_policy.activity_retries` override the global
  failed activity retry budget
- failed command retries preserve the original workflow command type and payload before adding
  retry metadata
- unknown successful activity/state pairs are preserved as completion events without unsafe state
  changes

Still intentionally not moved yet:

- implementation completion does not infer PR state from free-form agent text
- activity result schemas are not yet workflow-specific enough to drive every state transition
- retry policy supports bounded immediate retries only; backoff and cooldown are not yet modeled

Tests:

- reducer resumes implementation after replan completion
- reducer idles repo backlog workflows after dispatch and reconciliation completion
- reducer ignores unmapped successful activity completions
- reducer retries a failed activity until the configured retry budget is exhausted
- reducer honors activity-specific retry budget overrides
- reducer preserves `StartChildWorkflow` command type when retrying failed child workflow dispatch
- runtime worker applies the reducer, records decisions, and updates workflow state

### Phase 12: Workflow Runtime Profile Selection

Status: partially implemented.

Route runtime command dispatch through a workflow-aware profile selector instead of a single global
runtime profile.

Implemented now:

- `WORKFLOW.md` front matter can define `runtime_dispatch.workflow_profiles` overrides keyed by
  workflow definition id, `runtime_dispatch.activity_profiles` overrides keyed by activity name,
  and `runtime_dispatch.workflow_activity_profiles` overrides keyed first by workflow definition id
  and then by activity name
- command dispatch loads the command's workflow instance and activity name, then selects the most
  specific matching runtime profile when available
- selection precedence is workflow/activity pair, global activity, workflow, then configured default
  runtime profile
- server background dispatch builds the same profile selector from project workflow config

Still intentionally not moved yet:

- profile definitions are still lightweight config entries, not durable workflow definition
  artifacts

Tests:

- workflow config parses per-workflow, per-activity, and workflow/activity runtime profile overrides
- dispatcher uses workflow/activity overrides first, then activity overrides, then workflow
  definition overrides, then the default profile
- server dispatch config conversion preserves default, workflow override, activity override, and
  workflow/activity override behavior

### Phase 13: Runtime Profile Metadata Application

Status: partially implemented.

Carry runtime profile metadata beyond profile names and apply safe fields during server-owned
runtime execution.

Implemented now:

- `runtime_dispatch`, `runtime_dispatch.workflow_profiles`,
  `runtime_dispatch.activity_profiles`, and `runtime_dispatch.workflow_activity_profiles` can
  configure `model`, `reasoning_effort`, `sandbox`, `approval_policy`, `max_turns`, and
  `timeout_secs`
- profile overrides inherit default metadata for fields they do not set
- dispatcher persists the full selected `RuntimeProfile` into runtime job input
- runtime worker includes the parsed profile metadata in the prompt packet
- runtime worker passes `model` and `timeout_secs` into the turn lifecycle request

Still intentionally not moved yet:

- non-Codex approval policy still comes from registered agent/server configuration
- timeout is not a workflow reducer retry budget yet

Tests:

- workflow config parses runtime profile metadata fields
- dispatch profile selector inherits and overrides metadata correctly
- dispatcher preserves full profile metadata in runtime job input
- server worker passes a runtime profile model override to the agent request

### Phase 14: Runtime Profile Execution Timeout

Status: partially implemented.

Apply `timeout_secs` as a server lifecycle deadline so runtime profiles constrain ordinary
`CodeAgent` streaming execution as well as adapter-based turns.

Implemented now:

- turn lifecycle builds a hard execution deadline from `TurnLifecycleOptions::timeout_secs`
- deadline expiry marks the agent turn failed with a timeout-specific error item
- runtime worker completion converts the failed turn into a failed `ActivityResult`
- runtime job output records the timeout reason as structured activity error data

Still intentionally not moved yet:

- timeout policy is per runtime job, not a workflow-level retry or reducer budget
- non-Codex approval policy still requires request/runtime contract work

Tests:

- runtime worker fails a blocked registered agent when profile `timeout_secs` expires

### Phase 15: Runtime Profile Execution Metadata

Status: partially implemented.

Apply runtime profile metadata that already has a concrete execution-layer contract without moving
workflow policy into Rust code.

Implemented now:

- `AgentRequest` and adapter `TurnRequest` carry optional `reasoning_effort` and `sandbox_mode`
- runtime worker parses profile `sandbox` values and passes request metadata into turn lifecycle
- Codex exec uses request `reasoning_effort` for `model_reasoning_effort` and request sandbox for
  both CLI `-s` and the OS sandbox wrapper
- Codex app-server requests include profile `effort`, thread `sandbox`, and turn
  `sandboxPolicy` metadata
- Claude CodeAgent uses request `reasoning_effort` when no execution phase already defines effort
- Claude CodeAgent applies request sandbox to the OS sandbox wrapper

Still intentionally not moved yet:

- `approval_policy` is not applied to non-Codex runtime profiles because those adapters do not yet
  expose a matching approval contract
- Claude adapter sandbox wrapping is still unchanged because that path does not currently own a
  sandbox wrapper configuration

Tests:

- Codex exec base arguments honor request reasoning effort and sandbox overrides
- Codex app-server thread and turn payloads include runtime profile overrides
- Claude CodeAgent base arguments honor request reasoning effort without overriding execution phase
- server runtime worker passes profile reasoning effort and sandbox overrides to the agent request

### Phase 16: Runtime Profile Turn Budget

Status: partially implemented.

Apply `max_turns` as a workflow-runtime budget without changing agent CLI behavior. In the current
runtime contract each `RuntimeJob` maps to one agent turn, so the budget is enforced before a
claimed runtime job starts execution.

Implemented now:

- workflow runtime store can count started runtime turns for a workflow instance
- runtime worker checks the selected profile `max_turns` after claiming a job and before invoking
  the runtime executor
- exhausted budgets complete the job with a structured blocked `ActivityResult`
- blocked budget results mark the workflow command as `blocked` and flow through the existing
  runtime completion reducer
- currently running jobs from the same workflow count toward the budget, while the claimed current
  job is excluded so `max_turns: 1` allows the first turn and blocks the second

Still intentionally not moved yet:

- `max_turns` is not a multi-turn conversation budget inside a single runtime job because runtime
  jobs currently execute one lifecycle turn
- timeout policy is still per runtime job, not a workflow-level retry or reducer budget
- `approval_policy` is only applied to Codex runtime profiles

Tests:

- runtime worker executes the first job under `max_turns: 1`
- runtime worker blocks the second job for the same workflow without invoking the executor

### Phase 17: Runtime Profile Approval Policy

Status: partially implemented.

Apply `approval_policy` only where the execution layer has a concrete native contract. This keeps
workflow policy declarative while avoiding silent no-ops on adapters that do not yet expose approval
mode selection.

Implemented now:

- runtime worker validates profile `approval_policy` values against Codex native modes:
  `untrusted`, `on-failure`, `on-request`, and `never`
- runtime worker rejects `approval_policy` on non-Codex runtime kinds until those adapters define
  compatible semantics
- `AgentRequest` and adapter `TurnRequest` carry optional `approval_policy`
- Codex exec passes request approval policy via CLI `-a`
- Codex app-server passes request approval policy via `approvalPolicy` on both `thread/start` and
  `turn/start`
- Codex app-server turn sandbox metadata now uses the current `sandboxPolicy` object shape while
  thread startup keeps the native `sandbox` mode string

Still intentionally not moved yet:

- Claude approval behavior remains controlled by its existing permission/tooling path
- remote host runtimes must define their own runtime-host approval contract before using this field

Tests:

- Codex exec base arguments honor request approval overrides and keep the prompt as the final token
- Codex app-server thread and turn payloads include `approvalPolicy`
- runtime worker passes profile approval policy into the agent request
- runtime worker rejects unknown approval policies and non-Codex approval policy usage

## Test Strategy

Run after each implementation step:

- `cargo fmt --all`
- `cargo check -p harness-workflow`
- `cargo test -p harness-workflow`

Before any PR:

- `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets`

Expensive Postgres-heavy workspace tests may be skipped during local design iteration, but the
contract module must have full non-database unit coverage.
