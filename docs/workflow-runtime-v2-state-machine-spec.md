# Workflow Runtime V2 State Machine Spec

Status: Draft v1
Date: 2026-05-21
Compatibility: no backward compatibility with live legacy task orchestration
Audience: harness maintainers, runtime implementers, and reviewers

## Purpose

Harness currently has enough workflow-runtime pieces to run work, but the
state machine is not one coherent system yet. The same user-visible work can be
represented by legacy task rows, workflow instances, command rows, runtime
jobs, PR feedback children, and UI projections. That split allows a runtime job
to succeed while the workflow remains active, and it allows one API surface to
show active work while another reports zero.

V2 makes workflow runtime the only source of orchestration truth. It removes
live backward compatibility branches and defines the state, persistence,
recovery, projection, and verification contracts required for routine operation
to be mechanically provable.

This spec does not claim that agents, GitHub, the network, or local builds can
never fail. It requires that failures become explicit persisted states with
operator-visible evidence, and that a completed runtime job cannot silently
leave an active workflow behind.

## Audit Baseline

The design is based on a ten-lane static audit of the current runtime:

| Lane | Current finding | Evidence anchors |
|---|---|---|
| Runtime core | Structured `workflow_decision` artifacts can outrank stronger domain signals. Reducer commit is not atomic. `pr_feedback` child has unreachable `done` transitions. | `crates/harness-workflow/src/runtime/reducer.rs:51`, `crates/harness-workflow/src/runtime/worker.rs:222`, `crates/harness-workflow/src/runtime/validator.rs:291` |
| Admission | Runtime admission still depends on legacy task and issue workflow checks. Runtime handles can prefer old historical task IDs. Prompt bodies can be process-local only. | `crates/harness-server/src/services/execution.rs:431`, `crates/harness-server/src/workflow_runtime_submission.rs:22`, `crates/harness-server/src/workflow_runtime_submission.rs:323` |
| Dispatcher and worker | Jobs, commands, events, decisions, and instance state are committed through separate store calls. A terminal job can leave an active workflow if reducer returns no decision. | `crates/harness-workflow/src/runtime/store.rs:1262`, `crates/harness-workflow/src/runtime/worker.rs:202`, `crates/harness-workflow/src/runtime/worker.rs:243` |
| PR feedback | Review webhooks do not all enter the same feedback sweep path. Empty successful inspection can no-op. Child outcomes are non-terminal. | `crates/harness-server/src/webhook.rs:161`, `crates/harness-workflow/src/runtime/reducer.rs:1009`, `crates/harness-workflow/src/runtime/tests.rs:3737` |
| Repo backlog | `repo_backlog` is a long-lived state machine for discovery, can block polling, and is already documented as removable. | `docs/stateless-repo-backlog-poll-spec.md:31`, `crates/harness-server/src/workflow_runtime_repo_backlog.rs:73`, `crates/harness-server/src/stale_workflow_recovery.rs:4` |
| Projections | `/tasks` appends runtime summaries to legacy task summaries, while `/api/overview` still counts legacy `TaskQueue`. | `crates/harness-server/src/http/task_query_routes.rs:49`, `crates/harness-server/src/http/task_query_routes.rs:121`, `crates/harness-server/src/handlers/overview.rs:43` |
| Persistence | Runtime schema has weak graph constraints, no one-job-per-command database invariant, JSON-only job lease fields, and unbounded instance scans. | `crates/harness-workflow/src/runtime/store.rs:42`, `crates/harness-workflow/src/runtime/store.rs:76`, `crates/harness-workflow/src/runtime/store.rs:588` |
| Recovery | Runtime reconciliation misses closed issues before PR binding. Stale recovery mutates `repo_backlog` state directly instead of reconciling commands and jobs. | `crates/harness-server/src/reconciliation.rs:177`, `crates/harness-server/src/reconciliation.rs:454`, `crates/harness-server/src/stale_workflow_recovery.rs:85` |
| Tests | Current DB-backed runtime tests can skip when Postgres is missing. There is no full HTTP submission to dispatcher to worker to reducer to projection E2E. | `crates/harness-server/src/test_helpers.rs:83`, `crates/harness-server/src/http/tests.rs:3320`, `crates/harness-server/src/http/mod.rs:147` |
| Operations | `/health` does not prove workflow runtime health. Worker concurrency is batch-based. Status defaults can miss old stuck workflows. | `crates/harness-server/src/http/misc_routes.rs:43`, `crates/harness-server/src/http/background.rs:1495`, `crates/harness-cli/src/commands.rs:129` |

## Goals

- Make `workflow_runtime` the only orchestration source for issues, prompts,
  PR feedback, runtime jobs, status, and dashboard projections.
- Remove live legacy task compatibility from runtime admission and projection.
- Delete the long-lived `repo_backlog` workflow. GitHub issue discovery becomes
  stateless server intake that creates `github_issue_pr` workflows directly.
- Make runtime completion atomic: job completion, command status, workflow
  event, decision record, command outbox, inline side effects, and instance
  state update commit together.
- Make every known activity reducer domain-owned. Agent decisions are advisory
  unless the activity is explicitly declared generic.
- Make semantic empty success impossible for known activities.
- Make startup and periodic recovery repair or escalate stale runtime state
  without silently rewriting state.
- Make `/tasks`, `/api/overview`, dashboard, CLI status, and runtime tree share
  one runtime projection contract.
- Make CI prove runtime behavior with non-skipping database and E2E tests.

## Non-Goals

- Preserve compatibility with old live legacy task rows for runtime workflows.
  Existing rows may be migrated or archived once, but V2 code must not keep live
  dual-read branches.
- Keep `repo_backlog` as a workflow definition.
- Keep `pr_feedback` as a long-lived independent state machine with
  non-terminal outcome states.
- Hide unavailable runtime storage behind empty counts or default policies.
- Change long-running server process ownership policy.

## Runtime Ownership

The only long-lived workflow definitions in V2 are:

- `github_issue_pr`: one workflow per tracked GitHub issue or PR-backed issue.
- `prompt_task`: one workflow per accepted prompt task.
- `quality_gate`: optional, only when it has independent persisted gate value.

The following are not long-lived workflow state machines in V2:

- GitHub backlog polling. It is stateless intake.
- PR feedback inspection. It is a parent workflow activity with durable
  inspection artifacts and command/job records.
- Dependency analysis. It is an optional batched activity that produces issue
  creation input, not a per-repo workflow.

`workflow_runtime_store` is mandatory when runtime features are enabled. Server
startup must fail fast, or the HTTP/API must report runtime unavailable and
refuse runtime submissions. It must not report zero active work when the runtime
store failed to open.

## State Model

### Global Rules

- Terminal states are `done`, `failed`, and `cancelled`. `passed` is terminal
  only for gate workflows.
- `failed` and `cancelled` are never ordinary scheduler candidates.
- Reopen-in-place is removed. A retry after terminal state creates a new
  workflow run or records an explicit recovery workflow with a new run ID.
- `blocked` is non-terminal but operator-owned. It requires evidence and a
  `RequestOperatorAttention` command.
- `running` is not a workflow lifecycle state. It is an execution projection
  derived only from an unexpired running runtime job lease.
- A workflow in an active lifecycle state must have one of:
  - an active command,
  - an active runtime job,
  - an external wait condition with a persisted reason and wake rule,
  - or a recent accepted decision that terminalized or blocked it.

If none of those is true, the workflow is stale and must be repaired or
escalated by recovery.

### `github_issue_pr`

Allowed states:

- `discovered`
- `awaiting_dependencies`
- `scheduled`
- `implementing`
- `pr_open`
- `awaiting_feedback`
- `addressing_feedback`
- `ready_to_merge`
- `blocked`
- `done`
- `failed`
- `cancelled`

Required transitions:

| From | Event or activity | To | Required evidence and commands |
|---|---|---|---|
| `discovered` | dependency gate unresolved | `awaiting_dependencies` | persisted dependency set and `Wait` |
| `discovered`, `awaiting_dependencies`, `scheduled` | ready to implement | `implementing` | `EnqueueActivity(implement_issue)` |
| `implementing` | implementation produced PR artifact | `pr_open` | `BindPr` with PR number, URL, head SHA when available |
| `implementing` | issue is externally closed or agent proves already resolved and GitHub confirms closed | `done` | `MarkDone`, GitHub issue state evidence, validation summary |
| `implementing` | successful agent turn without PR, closure proof, or explicit safe no-op | `blocked` | `MarkBlocked`, `RequestOperatorAttention` |
| `implementing` | retryable activity failure | `implementing` | retry command with bounded retry policy |
| `implementing` | non-retryable failure | `blocked` or `failed` | classification evidence |
| `pr_open`, `awaiting_feedback` | feedback inspection found actionable feedback | `addressing_feedback` | `EnqueueActivity(address_pr_feedback)` and feedback artifact IDs |
| `pr_open`, `awaiting_feedback` | no actionable feedback but not merge-ready | `awaiting_feedback` | `Wait` with next sweep reason |
| `pr_open`, `awaiting_feedback`, `addressing_feedback` | no actionable feedback, checks accepted, mergeable or intentionally skipped | `ready_to_merge` | review/check/mergeability evidence |
| `addressing_feedback` | address activity pushed or replied | `awaiting_feedback` | artifact with pushed head SHA or reply IDs |
| `addressing_feedback` | successful activity without push, reply, or justified no-op | `blocked` | `MarkBlocked`, `RequestOperatorAttention` |
| `ready_to_merge` | operator merge approval or external merged PR reconciliation | `done` | `MarkDone`, merge evidence |
| any non-terminal | explicit operator cancel | `cancelled` | `MarkCancelled` |

The reducer must not leave `implementing` active after `implement_issue`
succeeds unless another active command or explicit wait was produced in the
same atomic commit.

### `prompt_task`

Allowed states:

- `submitted`
- `awaiting_dependencies`
- `implementing`
- `blocked`
- `done`
- `failed`
- `cancelled`

Prompt bodies required for execution must be persisted. A process-local cache
may exist only as an optimization. If a prompt cannot be reconstructed after
restart, the workflow must enter `blocked` with a configuration error instead
of staying queued or running.

Prompt idempotency is explicit:

- `external_id` present: idempotent by project and external ID.
- `external_id` absent: every submission is a new workflow.

### PR Feedback Inspection

V2 removes `pr_feedback` as a long-lived child workflow. Feedback inspection is
an activity command on the parent `github_issue_pr` workflow.

All review-comment ingress paths must converge on the same parent transition:

- `issue_comment` on a PR
- `pull_request_review`
- `pull_request_review_comment`
- manual `POST /tasks` PR feedback requests
- periodic sweep

Inspection output must include one explicit outcome:

- `FeedbackFound`
- `ChangesRequested`
- `ChecksFailed`
- `NoFeedbackFound`
- `PrReadyToMerge`

Empty successful inspection is invalid. The reducer must block with operator
attention when the activity succeeds without an outcome signal.

Feedback artifacts must persist:

- PR number and URL
- head SHA inspected
- unresolved review thread IDs
- top-level comment IDs considered actionable
- check suite names and conclusions
- mergeability state, when available
- stale or outdated classification
- body digests for dedupe, not full duplicated comment text unless already
  stored as part of the existing runtime log

Dedupe keys must include PR number, inspected head SHA, and feedback digest.
There must be no blanket time-based suppression that hides fresh actionable
feedback.

### Stateless GitHub Backlog Intake

The `repo_backlog` workflow definition is deleted. Intake directly fetches
GitHub issues through the configured token and creates `github_issue_pr`
workflows idempotently.

The existing design in `docs/stateless-repo-backlog-poll-spec.md` is accepted
as the starting point with these V2 additions:

- Intake counters are part of runtime health: `open_seen`, `skipped_pr`,
  `skipped_label`, `covered_existing`, `terminal_skipped`, `started_direct`,
  `dependency_analysis_queued`, and `errors`.
- `covered_existing` is not running work.
- `force_execute` label behavior is preserved when creating
  `github_issue_pr`.
- Failed or cancelled existing issue workflows are skipped unless an explicit
  operator retry creates a new run.
- Old `repo_backlog` rows are removed by migration or archived outside the
  active runtime schema.

## Reducer Policy

Known activities are server-owned. The reducer order for known activities is:

1. Parse and validate the `ActivityResult` envelope.
2. Verify the result activity matches the command activity.
3. Extract required domain signals and artifacts for the activity.
4. Apply domain precedence. Blocking feedback beats ready-to-merge. GitHub
   closure evidence beats agent text. Missing required signals is invalid.
5. Build a server-owned `WorkflowDecision`.
6. Record any conflicting agent `workflow_decision` artifact as rejected
   evidence.
7. Validate the server-owned decision against the state graph.
8. Commit atomically.

Generic `workflow_decision` artifacts are accepted only for activities whose
definition explicitly declares `decision_mode = "agent_owned"`. Runtime issue,
prompt, backlog, and PR feedback activities must not be agent-owned.

The reducer must never perform a silent fallback state transition for a known
activity without machine-checkable evidence.

## Persistence Contract

Runtime persistence must encode the graph in the database, not only in Rust
call order.

Required schema changes:

- Add foreign keys where practical:
  - `workflow_events.workflow_id -> workflow_instances.id`
  - `workflow_decisions.workflow_id -> workflow_instances.id`
  - `workflow_commands.workflow_id -> workflow_instances.id`
  - `workflow_commands.decision_id -> workflow_decisions.id`
  - `runtime_jobs.command_id -> workflow_commands.id`
  - `runtime_events.runtime_job_id -> runtime_jobs.id`
- Add `UNIQUE(runtime_jobs.command_id)` for commands that execute through one
  runtime job.
- Promote runtime job lease fields to columns:
  - `lease_owner`
  - `lease_expires_at`
- Index runtime job claiming by `(status, not_before, lease_expires_at,
  created_at)`.
- Add first-class runtime projection indexes for:
  - `definition_id, state, updated_at`
  - `project_id, repo, issue_number`
  - `project_id, repo, pr_number`
  - current task handle only
- Stop choosing the oldest historical `task_ids` entry as canonical. The
  current handle is `data.task_id`; historical handles are aliases only.
- Add real cursor pagination for workflow instance queries. No runtime API or
  background loop may call unbounded `list_instances_by_definition(None)`.

Required store API changes:

- Replace separate completion calls with one transaction:
  `commit_runtime_activity_completion`.
- The transaction must:
  1. verify current runtime job lease,
  2. mark the job terminal,
  3. mark the command terminal,
  4. append `RuntimeJobCompleted`,
  5. reduce and validate the decision,
  6. record accepted or rejected decision,
  7. enqueue follow-up commands,
  8. apply inline side effects,
  9. update workflow instance state and version,
  10. write projection rows.
- If any step fails, the transaction rolls back and the job remains eligible
  for recovery.
- Public unvalidated runtime job insertion is removed or made test-only.

## Recovery Contract

Recovery must be eventful. It must not mutate workflow state directly without a
decision record and evidence.

Startup and periodic recovery must detect:

- terminal runtime job with active workflow and no applied reducer decision,
- terminal command with active workflow and no active successor,
- dispatched command with no runtime job,
- pending runtime job with missing command,
- expired running runtime job lease,
- active workflow whose required command is missing,
- closed GitHub issue before PR binding,
- merged PR,
- closed unmerged PR,
- prompt workflow whose persisted prompt body is missing,
- workflow whose projection row disagrees with stored JSON or events.

Required recovery actions:

- Replay the missing `RuntimeJobCompleted` reduction when enough data exists.
- Recreate a missing runtime job for a valid dispatched command.
- Reset an expired running job to pending only through the lease protocol.
- Mark externally closed issue workflows `done` or `cancelled` with GitHub
  evidence.
- Mark merged PR workflows `done` with merge evidence.
- Block with operator attention when the runtime cannot infer a safe repair.
- Create a new explicit retry run for terminal workflows instead of reopening
  the terminal instance.

The old `stale_workflow_recovery` reset-to-idle behavior is removed with
`repo_backlog`.

## Projection Contract

Runtime projection is a shared module, not a per-route reconstruction.

Required consumers:

- `GET /tasks`
- `GET /tasks/{id}`
- `/api/overview`
- dashboard active and history views
- `/api/intake`
- `harness status`
- runtime tree

The projection must separate lifecycle and execution:

| Projection field | Source |
|---|---|
| lifecycle state | `workflow_instances.state` |
| execution state | active command and runtime job status |
| running count | unexpired running runtime jobs |
| queued count | pending runtime jobs plus dispatchable pending commands |
| blocked count | workflows in `blocked` or failed commands requiring operator action |
| done/failed/cancelled | terminal workflow states |
| stale count | recovery detector output |

If runtime projection is unavailable, APIs must return an explicit degraded or
unavailable response. They must not return zero active work as a fallback.

Legacy task rows may be migrated into archived history, but runtime projection
must not live-union legacy and runtime rows.

## Operations Contract

`/health` must include runtime state, not only HTTP/store readiness:

- runtime store availability,
- dispatcher enabled and last successful tick,
- worker enabled and last successful tick,
- active worker capacity,
- pending commands by type,
- runtime jobs by status,
- expired running leases,
- stale active workflows,
- projection lag,
- last recovery tick result.

Worker concurrency must be a continuously replenished bounded pool. A long job
must occupy one worker slot without delaying other finished slots from claiming
more work.

`WORKFLOW.md` load errors must be consistent:

- startup/runtime loops must fail closed or mark runtime unavailable,
- HTTP submission must fail with the same configuration error,
- default-enabled runtime behavior must not silently replace a broken project
  workflow config.

`harness status` must default to enough runtime visibility for operators, and
must print when its limit hides older workflows.

## Verification Matrix

CI must run database-backed runtime tests. Tests may skip locally when a
developer lacks Postgres, but CI must fail if the runtime DB test harness is not
available.

Required tests:

| Area | Required proof |
|---|---|
| Reducer | `implement_issue` success with PR moves to `pr_open`; success with GitHub-closed issue moves to `done`; success without PR or closure blocks. |
| Reducer | PR feedback blocking signals beat ready signals. Empty feedback success blocks. |
| Reducer | `address_pr_feedback` success without push, reply, or justified no-op blocks. |
| Reducer | Terminal workflows cannot reopen in place. |
| Store | `commit_runtime_activity_completion` is atomic under injected failures after job update, command update, decision record, and command enqueue. |
| Store | one runtime job per command is enforced by the database. |
| Store | expired lease reclaim uses indexed columns and stale worker completion is ignored. |
| Admission | issue, prompt, and PR feedback submissions require runtime store and do not inspect legacy task rows. |
| Admission | prompt dependency release survives process restart because prompt body is persisted. |
| Intake | stateless GitHub poll creates workflows idempotently and never creates `repo_backlog` rows. |
| PR feedback | all webhook types converge on parent feedback inspection. |
| Recovery | terminal job plus active workflow is replayed or blocked on restart. |
| Recovery | closed issue without PR binding exits active state. |
| Projection | `/tasks`, `/api/overview`, dashboard, and status report the same active/running/queued counts from runtime data. |
| E2E | HTTP submit -> dispatcher -> worker -> reducer -> projection completes for issue and prompt workflows. |
| E2E | unresolved review comment -> inspect -> address -> rescan -> ready_to_merge. |

## Migration Strategy

This is a breaking runtime migration.

1. Stop the server.
2. Run schema migrations for runtime V2.
3. Archive or delete old `repo_backlog` workflow rows, commands, decisions,
   events, and jobs.
4. Migrate current runtime issue and prompt handles to the V2 canonical handle
   format.
5. Archive legacy task rows that correspond to runtime workflows.
6. Start server with runtime store mandatory.
7. Run startup recovery.
8. Verify projection parity and runtime health before admitting new work.

Old binaries are unsupported after the V2 migration.

## Implementation Order

1. Add failing tests for the known production failure: succeeded runtime job,
   `github_issue_pr` still `implementing`, no active child process, closed
   issue or no PR.
2. Add atomic runtime completion transaction.
3. Make known activity reducers server-owned and block semantic empty success.
4. Remove `pr_feedback` child lifecycle and converge all feedback ingress on
   parent inspection.
5. Replace `repo_backlog` with stateless GitHub intake.
6. Persist prompt bodies and fix runtime handle canonicalization.
7. Build the shared runtime projection module and move `/tasks`,
   `/api/overview`, dashboard, and status onto it.
8. Replace direct stale state mutation with eventful recovery.
9. Add runtime health fields and continuous worker pool semantics.
10. Remove live legacy task compatibility branches and archive old rows.

Each step must land with focused tests and `cargo check`, `cargo test`, and
`RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` before PR.
