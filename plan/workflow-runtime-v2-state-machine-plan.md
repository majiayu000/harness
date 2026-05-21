# Workflow Runtime V2 State Machine Plan

Status: draft
Spec: `docs/workflow-runtime-v2-state-machine-spec.md`
Compatibility: no backward compatibility with live legacy task orchestration

## Scope

This plan turns the runtime-v2 spec into implementation slices. The immediate
goal is not another timeout or dashboard patch. The goal is to make the
workflow runtime's state transitions, command/job graph, recovery, projection,
and tests one coherent system.

## Baseline

- Current branch: `main`
- Pre-existing local edits before this plan: `WORKFLOW.md`, `start-server.sh`
- Current runtime problem class: succeeded terminal runtime jobs can leave
  active workflow instances, and UI/API projections can disagree about active
  work.
- User requirement: no live backward compatibility requirement; prefer a
  complete long-term design.

## Findings To Fix

| Priority | Finding | Files |
|---|---|---|
| P0 | Runtime job completion is not atomic with workflow advancement. | `crates/harness-workflow/src/runtime/worker.rs`, `crates/harness-workflow/src/runtime/store.rs` |
| P0 | Known activity reducers allow no-op success or agent-owned decisions where server-owned domain evidence is required. | `crates/harness-workflow/src/runtime/reducer.rs`, `crates/harness-workflow/src/runtime/validator.rs` |
| P0 | Runtime projection is split across legacy task rows, runtime summaries, overview counters, dashboard, and status. | `crates/harness-server/src/http/task_query_routes.rs`, `crates/harness-server/src/handlers/overview.rs`, `crates/harness-cli/src/commands/status.rs` |
| P1 | PR feedback ingress and child states can strand actionable feedback. | `crates/harness-server/src/webhook.rs`, `crates/harness-server/src/workflow_runtime_pr_feedback.rs`, `crates/harness-workflow/src/runtime/pr_feedback.rs` |
| P1 | `repo_backlog` is a removable long-lived discovery state machine. | `crates/harness-server/src/workflow_runtime_repo_backlog.rs`, `crates/harness-workflow/src/runtime/repo_backlog.rs`, `crates/harness-server/src/http/background.rs` |
| P1 | Prompt runtime execution is not restart-safe when prompt bodies are process-local. | `crates/harness-server/src/workflow_runtime_submission.rs`, `crates/harness-server/src/workflow_runtime_worker/data_helpers.rs` |
| P1 | Recovery mutates state or misses runtime cases instead of replaying or escalating through decisions. | `crates/harness-server/src/reconciliation.rs`, `crates/harness-server/src/stale_workflow_recovery.rs` |
| P2 | Runtime health and worker concurrency are insufficient for operator proof. | `crates/harness-server/src/http/misc_routes.rs`, `crates/harness-server/src/http/background.rs`, `crates/harness-core/src/config/workflow.rs` |

## Execution Plan

### P0.1 Failing Invariant Tests

Status: in_progress

Add tests that reproduce the production failure class:

- `implement_issue` succeeds with no PR but GitHub issue is closed, and the
  workflow must become `done`.
- `implement_issue` succeeds with no PR and no closure proof, and the workflow
  must become `blocked`.
- Runtime job terminal state plus active workflow is detected after restart.
- `/tasks` and `/api/overview` agree on active/running/queued counts for a
  runtime-only workflow.

Completed:

- `implement_issue` success without PR or terminal evidence now has reducer and
  worker coverage and moves the workflow to `blocked`.
- `implement_issue` success with structured closed-issue evidence now has
  reducer, worker, and prompt-contract coverage and moves the workflow to
  `done`.

Validation:

- `cargo test --package harness-workflow runtime_completion`
- `cargo test --package harness-server runtime_projection`

### P0.2 Atomic Completion Commit

Status: completed

Implement `WorkflowRuntimeStore::commit_runtime_activity_completion` and move
worker completion to that transaction.

Files:

- `crates/harness-workflow/src/runtime/store.rs`
- `crates/harness-workflow/src/runtime/worker.rs`
- `crates/harness-workflow/src/runtime/tests.rs`

Validation:

- worker completion now uses `commit_runtime_activity_completion_if_owned`,
  which verifies the lease and commits runtime job status, command status,
  `RuntimeJobCompleted`, reducer decision, follow-up commands, inline side
  effects, and workflow state together.
- `cargo test --package harness-workflow runtime_worker`.

### P0.3 Server-Owned Reducers

Status: pending

Make known activities use server-owned reducer decisions. Agent
`workflow_decision` artifacts become evidence unless the activity explicitly
declares agent-owned decisions.

Files:

- `crates/harness-workflow/src/runtime/reducer.rs`
- `crates/harness-workflow/src/runtime/validator.rs`
- `crates/harness-server/src/workflow_runtime_worker/activity_contract.rs`

Validation:

- semantic empty success blocks for implementation, PR feedback, prompt task,
  and dependency analysis,
- blocking PR feedback beats ready-to-merge,
- terminal reopen denied.

### P0.4 Runtime Projection Module

Status: pending

Create one runtime projection module and route `/tasks`, `/api/overview`,
dashboard, and CLI status through it.

Files:

- `crates/harness-server/src/runtime_projection.rs`
- `crates/harness-server/src/http/task_query_routes.rs`
- `crates/harness-server/src/handlers/overview.rs`
- `crates/harness-server/src/handlers/dashboard.rs`
- `crates/harness-cli/src/commands/status.rs`

Validation:

- runtime-only projection tests,
- cursor pagination tests,
- degraded runtime-store unavailable tests.

### P1.1 PR Feedback V2

Status: pending

Remove `pr_feedback` as a long-lived state machine. Convert inspection to a
parent `github_issue_pr` activity and converge all review webhook paths onto
that activity.

Files:

- `crates/harness-server/src/webhook.rs`
- `crates/harness-server/src/workflow_runtime_pr_feedback.rs`
- `crates/harness-workflow/src/runtime/pr_feedback.rs`
- `crates/harness-workflow/src/runtime/reducer.rs`
- `crates/harness-server/src/http/tests.rs`

Validation:

- `issue_comment`, `pull_request_review`, and `pull_request_review_comment`
  all enqueue the same parent inspection path,
- empty inspection success blocks,
- unresolved thread IDs are persisted,
- address feedback and rescan can reach `ready_to_merge`.

### P1.2 Stateless Backlog Intake

Status: pending

Implement `docs/stateless-repo-backlog-poll-spec.md` under V2 constraints and
delete the `repo_backlog` workflow definition.

Files:

- `crates/harness-server/src/intake/github_backlog_poller.rs`
- `crates/harness-server/src/http/background.rs`
- `crates/harness-server/src/workflow_runtime_repo_backlog.rs`
- `crates/harness-workflow/src/runtime/repo_backlog.rs`
- `crates/harness-workflow/src/runtime/reducer.rs`
- `crates/harness-workflow/src/runtime/validator.rs`

Validation:

- idempotent poll test,
- no `repo_backlog` row creation,
- force-execute label preserved,
- second poll starts zero new workflows.

### P1.3 Restart-Safe Prompt Runtime

Status: pending

Persist prompt bodies or durable prompt packets and make dependency release
restart-safe.

Files:

- `crates/harness-server/src/workflow_runtime_submission.rs`
- `crates/harness-server/src/workflow_runtime_worker/data_helpers.rs`
- `crates/harness-workflow/src/runtime/prompt_task.rs`

Validation:

- prompt submitted, server store reopened, dependency released, worker can run,
- missing persisted prompt body blocks with configuration error.

### P1.4 Eventful Recovery

Status: pending

Replace direct stale state mutation with recovery decisions and replay.

Files:

- `crates/harness-server/src/reconciliation.rs`
- `crates/harness-server/src/stale_workflow_recovery.rs`
- `crates/harness-workflow/src/runtime/store.rs`

Validation:

- terminal job plus active workflow recovers,
- closed issue without PR binding exits active state,
- merged PR marks workflow done,
- unsafe repair blocks with operator attention.

### P2.1 Runtime Health And Worker Pool

Status: pending

Make `/health` and `harness status` prove workflow runtime health, and replace
batch-based worker concurrency with a continuously replenished bounded pool.

Files:

- `crates/harness-server/src/http/misc_routes.rs`
- `crates/harness-server/src/http/background.rs`
- `crates/harness-cli/src/commands/status.rs`
- `crates/harness-core/src/config/workflow.rs`

Validation:

- health reports store, dispatcher, worker, stale, command, and job status,
- one long job does not prevent free worker slots from claiming more work,
- status warns when output is limited.

### P2.2 Legacy Removal Migration

Status: pending

Remove live legacy task compatibility from runtime admission and projection.
Archive or migrate historical rows once.

Files:

- `crates/harness-server/src/services/execution.rs`
- `crates/harness-server/src/http/task_query_routes.rs`
- `crates/harness-server/src/task_runner/store.rs`
- migration files under the runtime store migration sequence

Validation:

- runtime submissions never register legacy task rows,
- runtime APIs do not live-union legacy rows,
- old runtime task aliases resolve only through the runtime alias table.

## Stop Conditions

Stop implementation and update this plan if:

- a migration would destroy user data without an archive path,
- CI cannot provide a non-skipping Postgres runtime test environment,
- a reducer change needs agent-owned decisions for a known activity,
- a runtime projection cannot explain why a workflow is counted active.

## Required Final Verification

Before the V2 PR can be considered merge-ready:

- `cargo fmt --all -- --check`
- `cargo test`
- `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets`
- runtime E2E tests with Postgres enabled
- manual smoke: start server from standalone terminal, submit one prompt and
  one issue, verify runtime health, projections, and terminal workflow state.

## Execution Log

| Date | Step | Evidence |
|---|---|---|
| 2026-05-21 | P0.1 partial: `implement_issue` success without PR evidence blocks instead of leaving `implementing`. | `cargo test --package harness-workflow runtime_completion_reducer_blocks_issue_implementation_success_without_pr`; `cargo test --package harness-workflow runtime_worker_blocks_implementation_success_without_pr_evidence` |
| 2026-05-21 | P0.2: worker runtime completion moved to atomic store commit. | `cargo test --package harness-workflow runtime_completion_reducer`; `cargo test --package harness-workflow runtime_worker` |
| 2026-05-21 | P0 package verification. | `cargo test --package harness-workflow`; `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` |
| 2026-05-21 | P0.1 partial: `implement_issue` success with closed issue evidence finishes the workflow. | `cargo test --package harness-workflow runtime_completion_reducer`; `cargo test --package harness-workflow runtime_worker`; `cargo test --package harness-server activity_result_schema_describes_issue_implementation_terminal_evidence_contract`; `cargo test --package harness-server runtime_job_worker_tick_runs_registered_agent_and_completes_job`; `cargo test` |
