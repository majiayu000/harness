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

Still intentionally not moved yet:

- existing task runner ownership of process execution
- repo backlog polling
- runtime worker claiming of outbox commands
- workflow-first dashboard rendering

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

Move GitHub polling and reconciliation into `repo_backlog`.

Tests:

- open issue without workflow emits start command
- merged PR updates the bound issue workflow
- stale active workflow emits recovery event

### Phase 5: Runtime Worker Abstraction

Convert existing Codex and Claude execution paths into runtime workers.

Tests:

- runtime worker claims one job once
- runtime events stream in sequence
- failed runtime job records structured error
- cancelled runtime job releases lease

### Phase 6: Workflow-First UI

Keep task drill-down, but make workflow tree the primary dashboard surface.

Tests:

- workflow tree displays parent and child workflows
- runtime jobs are visible under workflow activities
- rejected decisions show operator-readable reasons

## Test Strategy

Run after each implementation step:

- `cargo fmt --all`
- `cargo check -p harness-workflow`
- `cargo test -p harness-workflow`

Before any PR:

- `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets`

Expensive Postgres-heavy workspace tests may be skipped during local design iteration, but the
contract module must have full non-database unit coverage.
