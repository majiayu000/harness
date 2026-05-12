# Workflow Runtime Hardening Design

## Goal

The workflow runtime must remain correct when agent output is incomplete, stale, malformed, or semantically wrong. Agents are allowed to propose facts and decisions; the workflow runtime owns persisted state transitions, command creation, leases, retry policy, and operator escalation.

This design covers every workflow definition currently wired through the runtime reducer and validator:

| Workflow | Definition ID | Runtime activities | Primary completion contract |
|---|---|---|---|
| GitHub issue PR | `github_issue_pr` | implementation, feedback, replan, merge/recovery activities | Bind PR artifacts, feedback signals, or validated workflow decisions |
| Repo backlog | `repo_backlog` | `poll_repo_backlog`, `plan_repo_sprint`, child dispatch/reconciliation activities | Issue/sprint signals with valid issue numbers, or explicit no-op signals |
| PR feedback child | `pr_feedback` | `inspect_pr_feedback` | Feedback outcome signals |
| Prompt task | `prompt_task` | `implement_prompt_task` | Successful implementation result and `MarkDone` command |
| Quality gate | `quality_gate` | quality check activity | Successful check result or blocked/failed result |

## Core Invariants

1. **The server owns state.** Agent output is candidate input. A persisted workflow state changes only after a `WorkflowDecision` validates against the workflow instance, transition allowlist, required command rules, lease context, and terminal-state policy.
2. **Every completed agent turn must return structured output.** A completed turn without a `harness-activity-result` fenced JSON block is a configuration failure. A malformed or mismatched structured block is also a configuration failure. Configuration failures are not retryable by default because repeating the same prompt/adapter contract is expected to fail again.
3. **Structured `workflow_decision` artifacts are never trusted blindly.** A valid structured decision wins. An invalid structured decision is converted into a deterministic `blocked` decision with `MarkBlocked` and `RequestOperatorAttention` unless a workflow-specific reducer can derive a valid fallback from structured signals or artifacts.
4. **Semantic empty success is not success.** Discovery and planning activities must emit either actionable signals or explicit empty/no-op signals. For example, `poll_repo_backlog` must emit `IssueDiscovered`, `IssueSkipped`, or `NoOpenIssueFound`; `plan_repo_sprint` must emit `SprintTaskSelected`, `IssueSkipped`, `NoSprintTaskSelected`, or a valid sprint plan artifact.
5. **Transitions that require work must create commands.** The validator enforces required command types for workflow transitions such as repo backlog scan, sprint planning, and child workflow dispatch. A state move without the command that makes the next state progress is rejected.
6. **Terminal states are not ordinary scheduler candidates.** Normal sweepers must not reopen `failed`, `done`, or `cancelled` workflows. Recovery from a terminal state requires an explicit recovery path and validation context.
7. **Runtime job completion is lease-owned.** A worker may complete a runtime job only while it still owns the exact running job lease. Late output from an expired lease is ignored and cannot overwrite a reclaimed job, even when the next worker reuses the same owner name.

## Agent Output Boundary

The only stable boundary between an agent runtime and the workflow runtime is `ActivityResult`.

Valid successful activity results should use:

- `status: "succeeded"` only when the activity emitted all required domain signals/artifacts.
- Domain signals for observed facts, such as `IssueDiscovered`, `NoOpenIssueFound`, `FeedbackFound`, or `PrReadyToMerge`.
- Domain artifacts for durable objects, such as `pull_request`, `sprint_plan`, or `workflow_decision`.

Invalid output is classified as follows:

| Output problem | Runtime status | Error kind | Reducer behavior |
|---|---|---|---|
| Missing fenced result block | `failed` | `configuration` | Fail activity; no retry by default |
| Malformed fenced JSON | `failed` | `configuration` | Fail activity; no retry by default |
| Fenced result activity does not match expected activity | `failed` | `configuration` | Fail activity; no retry by default |
| Valid JSON but invalid workflow decision | `succeeded` input, reduced to `blocked` workflow decision | n/a | Mark workflow blocked and request operator attention |
| Discovery/planning success with no required signals | `succeeded` input, reduced to `blocked` workflow decision | n/a | Mark workflow blocked and request operator attention |
| Timeout or external dependency failure | `failed` | `timeout` / `external_dependency` | Retry only when workflow state and retry policy allow it |

## Reducer Policy

Reducer order is intentionally conservative:

1. Parse any structured `workflow_decision`.
2. If it validates, accept it.
3. Try workflow-specific domain reducers that derive decisions from stable signals and artifacts.
4. Apply workflow-specific semantic guards, such as repo backlog empty-success blocking.
5. If an invalid structured decision remains, convert it to `blocked`.
6. Apply narrow legacy success fallbacks for known activities.
7. Return no decision when the runtime cannot safely infer progress.

This preserves useful agent autonomy while preventing invalid proposals from being recorded as silent no-progress loops.

## Retry And Recovery

Retry is only for failures where repetition can plausibly succeed:

- Retryable error kinds: `retryable`, `timeout`, `external_dependency`, `unknown`, or no error kind.
- Non-retryable error kinds: `fatal` and `configuration`.
- Retry must be enabled by `runtime_retry_policy`.
- Same-state retry is allowed only for workflow states where re-enqueueing the same activity is safe.

Repo backlog scan now participates in same-state retry while it is actively `scanning`. A repo backlog workflow in `failed` is not a poll candidate; terminal recovery must be explicit.

## Lease Ownership

Runtime workers can reclaim expired running jobs. After reclaim, the old worker may still return output. The store now provides an owner-checked completion path:

1. Load the runtime job under row lock.
2. Verify `status == running`.
3. Verify `lease.owner == current worker`.
4. Verify `lease.expires_at == claimed lease expires_at`.
5. Complete the job only when all checks pass.
6. Return `None` for stale leases without mutating job output or command/workflow state.

The worker uses this owner-checked completion path before recording workflow completion.

## Observability

Blocked decisions include:

- The activity name.
- The runtime job ID when available.
- The reason the output could not safely advance the workflow.
- A `MarkBlocked` command for persisted workflow state.
- A `RequestOperatorAttention` command for operator-facing remediation.

The intended operator response is to inspect the prompt packet, activity result, runtime job events, and workflow state, then either fix the prompt/adapter/configuration or run an explicit recovery action.

## Verification Matrix

Required checks for this hardening layer:

| Area | Test expectation |
|---|---|
| Invalid structured decision | Reducer emits valid `blocked` decision instead of returning invalid agent proposal |
| Repo backlog empty success | Poll and sprint activities block when required signals/artifacts are missing |
| Repo backlog invalid issue signals | Poll/sprint activities block when selected/discovered issues have no valid `issue_number` |
| Required transition commands | Repo backlog scan/planning/dispatch transitions require the command that makes progress possible |
| Retry policy | Same-state retry works for allowed states, including repo backlog `scanning` |
| Terminal boundary | Repo backlog `failed` is not an ordinary poll candidate |
| Activity result parsing | Missing/malformed/mismatched structured results become configuration failures |
| Runtime job lease ownership | Stale worker completion is ignored after another worker reclaims the job |

## Non-Goals

This design does not make agents deterministic. It makes unstable output observable and bounded. It also does not start the harness server from inside a Codex session; server startup remains an operator action from a standalone terminal.
