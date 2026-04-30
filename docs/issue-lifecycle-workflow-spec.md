# Workflow-First Orchestration Spec

## Goal

Define a workflow-first orchestration model with two authoritative layers:

- `ProjectWorkflow` for repo-level orchestration
- `IssueWorkflow` for a single GitHub issue lifecycle

The combined model must own the full lifecycle of a GitHub issue:

- intake
- optional triage and planning
- implementation
- PR creation
- PR feedback handling
- merge readiness
- completion

This design must replace the current split-brain behavior where:

- `issue:N` and `pr:M` become independent task lines
- PR feedback handling depends on ad hoc webhook/task injection
- `PLAN_ISSUE` is treated as terminal failure instead of structured workflow feedback
- repo orchestration status is inferred from scattered queue/task state instead of explicit workflow state

The design must be workflow-driven, not hard-coded as a large nest of executor conditionals.

## Non-goals

- Replacing the existing agent execution pipeline in one step
- Rewriting `TaskPhase` or `TaskStatus` into a new enum family immediately
- Implementing a generic external workflow DSL before proving the shape in-process

## Design principles

1. The issue is the primary unit of orchestration.
2. A PR is an artifact of the issue workflow, not a separate top-level lifecycle.
3. `Triage`, `Plan`, `Implement`, and `Review` remain execution activities, not top-level business states.
4. Agent disagreement is evidence, not authority.
5. PR feedback must be swept automatically as part of workflow progression.
6. A user-controlled label must be able to force execution even when the agent raises a plan concern.

## Current gap

Today the system has:

- task execution phases: `Triage`, `Plan`, `Implement`, `Review`, `Terminal`
- task runtime statuses: `Pending`, `Implementing`, `Waiting`, `Reviewing`, `Done`, `Failed`, ...

These are useful executor-level mechanics, but they do not form a full issue lifecycle because they do not model:

- issue discovery vs scheduling
- issue to PR binding as a first-class relation
- PR feedback waiting vs PR feedback addressing
- replan as a non-terminal branch
- merge readiness
- force-execute label overrides

## Compatibility stance

This spec intentionally does **not** preserve the old “task-first as source of truth” model.

The authoritative state after this redesign is:

1. `ProjectWorkflow`
2. `IssueWorkflow`
3. executor task state only as an implementation detail

`TaskPhase` and `TaskStatus` may continue to exist during migration, but they are no longer the product-facing lifecycle contract.

## Proposed architecture

Introduce a workflow layer in `harness-workflow` with three concepts:

1. `WorkflowSpec`
2. `WorkflowInstance`
3. `WorkflowActivity`

The workflow layer owns business-state transitions.
The existing executor pipeline continues to own task execution details.

### Layer split

#### Workflow layer

Owns:

- project orchestration state
- issue lifecycle state
- event handling
- transition decisions
- PR binding
- feedback sweep policy
- replan policy

Does not own:

- prompt text
- agent process spawning
- checkpoint content format
- review loop implementation details

#### Executor layer

Owns:

- triage activity
- planning activity
- implementation activity
- review/fix activity
- checkpoint persistence
- PR detection

## Canonical workflow: `project_workflow`

### States

```text
idle
polling_intake
planning_batch
dispatching
monitoring
sweeping_feedback
paused
degraded
```

### State semantics

- `idle`
  - The repo has no active orchestration action in progress.

- `polling_intake`
  - Intake is actively discovering new issue or PR signals for the repo.

- `planning_batch`
  - The repo is building or running a batch/sprint plan for discovered issues.

- `dispatching`
  - The repo is actively creating or admitting issue workflows into execution.

- `monitoring`
  - The repo has active issue workflows and is waiting on progress/terminal signals.

- `sweeping_feedback`
  - The repo is sweeping open PR feedback and converting it into issue workflow signals.

- `paused`
  - Orchestration is intentionally paused, for example by maintenance windows.

- `degraded`
  - The repo workflow is alive but blocked by orchestration/runtime problems.

### Events

- `poll_started`
- `poll_completed`
- `sprint_planning_started`
- `sprint_planner_enqueued`
- `dispatch_started`
- `dispatch_completed`
- `monitoring_started`
- `feedback_sweep_started`
- `feedback_sweep_completed`
- `repo_paused`
- `repo_degraded`
- `repo_idle`

### Activities

- `poll_intake`
- `build_sprint_plan`
- `dispatch_issue_workflows`
- `monitor_issue_workflows`
- `sweep_repo_feedback`
- `reconcile_repo_state`

## Canonical workflow: `issue_workflow`

### States

```text
discovered
scheduled
implementing
pr_open
awaiting_feedback
feedback_claimed
addressing_feedback
ready_to_merge
blocked
done
failed
cancelled
```

### State semantics

- `discovered`
  - The issue is known to the system but not yet admitted for work.

- `scheduled`
  - The issue has been selected for execution and can claim runtime capacity.

- `implementing`
  - The issue is in active implementation, including optional triage/plan/replan work.

- `pr_open`
  - A PR is bound to this issue workflow instance.

- `awaiting_feedback`
  - The PR exists and the workflow is waiting for new actionable feedback or mergeability.

- `feedback_claimed`
  - Actionable PR feedback was claimed by the sweeper, but enqueue has not yet
    persisted a real PR task id.

- `addressing_feedback`
  - Actionable PR feedback has been detected and a fix round is active.

- `ready_to_merge`
  - The PR is green, no actionable feedback remains, and merge preconditions are satisfied.

- `blocked`
  - The workflow cannot proceed without external input, conflict resolution, or policy override.

- `done`
  - The issue is resolved and merged or otherwise completed successfully.

- `failed`
  - The workflow terminated unsuccessfully.

- `cancelled`
  - The workflow was intentionally cancelled.

## Events

### Intake and scheduling

- `issue_discovered`
- `issue_scheduled`
- `capacity_denied`

### Execution

- `triage_completed`
- `plan_completed`
- `implement_started`
- `implement_completed`
- `pr_detected`
- `implement_failed`
- `plan_issue_detected`

### PR feedback

- `feedback_sweep_completed`
- `feedback_found`
- `feedback_task_scheduled`
- `no_feedback_found`
- `changes_requested`
- `approved`
- `mergeable`
- `conflicting`
- `ci_failed`
- `ci_green`

### Terminal

- `merged`
- `workflow_failed`
- `workflow_cancelled`

## Activities

These are named workflow actions. They map to existing or incremental code paths.

- `run_triage`
- `run_plan`
- `run_replan`
- `run_implement`
- `bind_pr`
- `sweep_pr_feedback`
- `run_fix_round`
- `check_mergeability`
- `mark_blocked`
- `mark_failed`
- `mark_done`

## Transition table

### Discovery and admission

```text
discovered --issue_scheduled--> scheduled
scheduled --implement_started--> implementing
scheduled --capacity_denied--> scheduled
```

### Implementation path

```text
implementing --pr_detected--> pr_open
implementing --implement_failed--> failed
implementing --workflow_cancelled--> cancelled
```

### `PLAN_ISSUE` handling

```text
implementing --plan_issue_detected--> implementing
```

This event does not directly fail the workflow.
Instead it triggers a policy decision:

- if force-execute label is absent:
  - run `run_replan`
  - then run `run_implement`
- if force-execute label is present:
  - record a plan concern artifact
  - continue with `run_implement`

### PR-open path

```text
pr_open --feedback_sweep_completed--> awaiting_feedback
awaiting_feedback --feedback_found--> feedback_claimed
awaiting_feedback --mergeable--> ready_to_merge
awaiting_feedback --conflicting--> blocked
awaiting_feedback --ci_failed--> awaiting_feedback
awaiting_feedback --no_feedback_found--> awaiting_feedback
```

### Feedback loop

```text
feedback_claimed --feedback_task_scheduled--> addressing_feedback
feedback_claimed --no_feedback_found--> awaiting_feedback
addressing_feedback --implement_started--> addressing_feedback
addressing_feedback --pr_detected--> pr_open
addressing_feedback --implement_failed--> failed
```

### Completion

```text
ready_to_merge --merged--> done
blocked --workflow_failed--> failed
blocked --workflow_cancelled--> cancelled
```

## Authority model

### ProjectWorkflow owns

- repo intake progress
- repo planning progress
- repo dispatch progress
- repo monitoring / feedback sweep posture

### IssueWorkflow owns

- issue admission
- triage/planning/implementation branches
- issue-to-PR binding
- feedback addressing
- merge readiness
- terminal issue outcome

### Executor task state owns

- current turn execution details
- retry counters
- tool/process lifecycle
- implementation/review sub-phase execution

## Mapping rules

- One `ProjectWorkflow` exists per canonical project root / repo.
- One `IssueWorkflow` exists per `(project_id, issue_number)`.
- `pr:M` is never a top-level lifecycle; it routes into the already-bound `IssueWorkflow`.
- A single `ProjectWorkflow` may coordinate many `IssueWorkflow`s.

## Product-facing implications

After this redesign, dashboards and operator APIs should prefer:

1. `ProjectWorkflow.state`
2. `IssueWorkflow.state`

and only use `TaskStatus` / `TaskPhase` for drill-down diagnostics.

## Force-execute label policy

Introduce a label contract:

- `force-execute`

Semantics:

- When present on the GitHub issue, agent-raised `PLAN_ISSUE` cannot block progression.
- The system records the concern but must continue execution against the issue contract.
- This label does not override:
  - merge conflicts
  - missing repository access
  - missing workspace path
  - missing required runtime resources

This keeps product authority with the operator while still preserving agent evidence.

## PR binding model

Introduce a durable issue-to-PR binding record:

```text
issue_key -> pr_number
issue_key -> pr_url
issue_key -> workflow_instance_id
```

Rules:

1. Once `issue:N` detects `pr:M`, that PR becomes the canonical PR for the issue workflow.
2. A later `pr:M` signal must route into the same workflow instance instead of creating a new top-level lifecycle.
3. A later issue retry for the same `issue:N` must reuse the existing PR binding when possible.

This is the main fix for the current `issue:N` vs `pr:M` split.

## PR feedback sweep

This must be a first-class activity, not an incidental webhook side effect.

### Inputs

- PR top-level comments
- review summaries
- inline review comments
- review state
- CI state
- mergeability state

### Outputs

- structured actionable feedback list
- workflow events:
  - `feedback_found` (claim placeholder persisted)
  - `feedback_task_scheduled`
  - `no_feedback_found`
  - `approved`
  - `changes_requested`
  - `ci_failed`
  - `ci_green`
  - `conflicting`
  - `mergeable`

### Triggering

Trigger the sweep from both:

1. webhook signals, when configured
2. periodic reconciliation polling, as a backstop

This avoids relying on webhook correctness as the sole orchestration path.

## Persistence model

Add a workflow state store in `harness-workflow`.

Suggested minimal persisted fields:

```text
workflow_id
workflow_type
state
issue_repo
issue_number
pr_number
pr_url
task_id
last_event
last_transition_at
labels_snapshot
attempt_count
blocked_reason
plan_concern
```

This is intentionally issue-centric, not task-centric.

## Relationship to existing `TaskPhase`

`TaskPhase` remains valid but moves down one layer.

Example mapping:

- workflow state `implementing`
  - activity may run:
    - `TaskPhase::Triage`
    - `TaskPhase::Plan`
    - `TaskPhase::Implement`

- workflow state `addressing_feedback`
  - activity may run:
    - `TaskPhase::Implement`
    - `TaskPhase::Review`

This avoids conflict between the new workflow and current phase enums.

## Relationship to existing `TaskStatus`

`TaskStatus` remains an execution status for the currently attached task.
It should not be treated as the full business-state source of truth for the issue.

The workflow state becomes authoritative for lifecycle reporting.

## Suggested implementation plan

### Phase 1: internal workflow engine, static spec

Add to `harness-workflow`:

- `workflow_spec.rs`
- `workflow_runtime.rs`
- `workflow_state_store.rs`

Use a static Rust table for the `issue_lifecycle` workflow first.
Do not introduce YAML parsing yet.

### Phase 2: bind issue and PR to one workflow instance

Replace current independent lifecycle handling with:

- issue admission creates workflow instance
- PR detection updates workflow instance
- PR webhook/poll events route into the same workflow instance

### Phase 3: move `PLAN_ISSUE` into workflow event handling

Replace:

- `PLAN_ISSUE -> TaskStatus::Failed`

With:

- `PLAN_ISSUE -> workflow event`
- policy branch:
  - replan
  - or force-execute continue

### Phase 4: add PR feedback sweep polling

Add periodic PR reconciliation so the system can progress even when webhook delivery is missing or incomplete.

### Phase 5: optional declarative spec files

Once the runtime shape is stable, move the static spec into:

- `workflows/issue_lifecycle.yaml`

The runtime stays generic; only the spec moves out of code.

## Why static spec before YAML

The current repository does not yet contain a generic workflow runtime.
Jumping straight to an external DSL would increase implementation risk.

A static internal spec:

- keeps the first change set smaller
- makes debugging easier
- proves the state/event/activity boundaries
- avoids inventing configuration syntax too early

The long-term goal can still be a file-driven workflow.

## Operator-facing behavior after this change

After this workflow exists, the operator experience should become:

1. Submit issue once.
2. The system owns the issue lifecycle.
3. If a PR is created, feedback handling stays attached to the same issue workflow.
4. If the agent raises a plan concern:
   - default: the system replans
   - with `force-execute`: the system continues anyway
5. The operator no longer needs to manually resubmit PR tasks one by one.

## Acceptance criteria

1. `issue:N` and `pr:M` no longer create two independent top-level lifecycles when they refer to the same work.
2. `PLAN_ISSUE` no longer directly maps to terminal task failure.
3. PR feedback is processed without requiring manual resubmission of the PR as a new task.
4. A `force-execute` label prevents agent-raised plan concerns from blocking implementation.
5. Workflow state survives restart independently from ephemeral executor state.
6. Dashboard views can report workflow lifecycle state separately from task execution status.

## Open questions

1. Should merge be performed automatically from `ready_to_merge`, or remain manual?
2. Should `approved` immediately imply `ready_to_merge`, or must CI and mergeability be revalidated every time?
3. Should `blocked` split into:
   - `blocked_on_conflict`
   - `blocked_on_policy`
   - `blocked_on_external_input`
4. Should `force-execute` live on the issue, the PR, or both?

## Recommended next step

Implement Phase 1 only:

- internal workflow runtime
- issue lifecycle state store
- issue-to-PR binding
- `PLAN_ISSUE` as workflow event

Do not attempt full YAML-driven workflows in the first patch.
