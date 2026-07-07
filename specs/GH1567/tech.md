# Tech Spec

## Linked Issue

GH-1567

## Product Spec

`specs/GH1567/product.md`

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| GitHub issue coverage gate | `crates/harness-server/src/intake/github_coverage_gate.rs` | `runtime_issue_state_is_covered` treats `blocked`, `failed`, and `cancelled` as covered, so intake skips the issue. | This is the direct source of the stuck poll behavior. |
| Runtime failure reducer | `crates/harness-workflow/src/runtime/reducer/runtime_failure.rs` | Failed decisions persist a reason and optional `error_kind` in command payloads; blocked decisions mark blocked with a free-text reason. | Stop reasons must become structured and visible on the instance/API. |
| Activity result model | `crates/harness-workflow/src/runtime/model.rs` | `ActivityResult` carries `summary`, optional `error`, optional `error_kind`, artifacts, signals, and validation records. | This is the source material for structured stop metadata. |
| Runtime instance store | `crates/harness-workflow/src/runtime/store/instances.rs` | `WorkflowRuntimeStore` can load and upsert instances and applies decision transitions. | Operator actions need atomic state updates and audit events. |
| Runtime HTTP routes | `crates/harness-server/src/http/http_router.rs`, `task_mutation_routes.rs` | Runtime merge and cancel actions exist as body-based routes: `/api/workflows/runtime/merge` and `/api/workflows/runtime/cancel`. There is no unblock or retry route. | New recovery routes should follow the existing runtime-action style. |
| Runtime tree API | `crates/harness-server/src/http/misc_routes_runtime_tree.rs` | Runtime instances, commands, jobs, events, and summaries are exposed to the dashboard. | Structured reasons should be visible here. |
| Operator monitor | `crates/harness-server/src/handlers/operator_monitor.rs`, `web/src/routes/overview/OperatorMonitorPanel.tsx` | Aged stuck workflows are visible, but the surface does not expose actionable unblock/retry details. | This is the primary operator UI for stopped workflows. |
| Usage guide | `docs/usage-guide.md` | Documents watchdog and retention, but not an operator unblock/retry workflow. | Operators need the supported flow documented. |

## Proposed Design

### Stop-Reason Metadata

Add a small structured stop-reason payload stored in workflow instance `data`.
Use additive JSON fields so no schema migration is required:

```json
{
  "blocked_reason": "SpecRail task SP841-T2 requires maintainer approval",
  "unblock_hint": "Post the approval comment on the GitHub issue, then call unblock.",
  "failure_reason": "Codex proxy returned a transient transport error",
  "retry_hint": "Retry after the proxy route is healthy.",
  "last_stop": {
    "state": "blocked",
    "activity": "implement_issue",
    "runtime_job_id": "runtime-job-id",
    "event_id": 123,
    "error_kind": "configuration",
    "recorded_at": "2026-07-05T17:13:16Z"
  }
}
```

Reducers that transition a workflow into `blocked` or `failed` should populate
the relevant fields from `ActivityResult.error`, `summary`, `error_kind`,
signals, and runtime completion event metadata. Existing rows without these
fields remain valid; APIs should fall back to current event/result text.

### Operator Actions

Add body-based runtime actions to match the existing merge/cancel routes and
avoid path encoding problems with workflow ids that contain project paths and
slashes:

- `POST /api/workflows/runtime/unblock`
- `POST /api/workflows/runtime/retry`

Request shape:

```json
{
  "workflow_id": "...",
  "reason": "operator supplied the requested approval comment"
}
```

Response shape on success for unblock:

```json
{
  "status": "unblocked",
  "execution_path": "workflow_runtime",
  "workflow_id": "...",
  "previous_state": "blocked",
  "state": "discovered"
}
```

Response shape on success for retry:

```json
{
  "status": "retried",
  "execution_path": "workflow_runtime",
  "workflow_id": "...",
  "previous_state": "failed",
  "state": "discovered"
}
```

The issue text proposed path-style endpoints such as
`/api/workflows/instances/{id}/unblock`. Those can be added as aliases only if
implementation includes URL-encoding tests for workflow ids containing `/`.
The first implementation should prefer body-based routes for consistency with
existing runtime merge/cancel.

### State Transition Semantics

`unblock`:

- Allowed only from `blocked`.
- Clears the coverage hold by moving the instance to a dispatchable state.
- For GitHub issue workflows, the target state should be the earliest safe
  re-dispatch state already accepted by the definition, such as `discovered` or
  another existing pre-dispatch state chosen by the implementation after
  checking the transition registry.
- Preserves historical stop metadata under `last_stop` or an append-only audit
  event; it must not erase evidence.

`retry`:

- Allowed only from `failed`.
- Reuses the same dispatchable-state strategy as `unblock` when retry is
  operator-approved.
- Must reject non-retryable `error_kind` values such as `fatal` and
  `configuration` unless the request explicitly includes a force flag and a
  follow-up spec approves that force behavior.
- If `error_kind` is missing or null, such as in legacy failed workflows created
  before this feature, treat the workflow as retryable by default and rely on
  the operator-supplied reason plus the next runtime result to record fresh
  evidence. The first implementation should omit force retry.

`cancelled`:

- No action in the first implementation. Return `409 Conflict` with the current
  state if called on a cancelled workflow.

Both actions must execute the state update and audit write inside one database
transaction. The transaction must insert a workflow event or decision record
that includes the operator-supplied reason, previous state, next state, and
actor/source identifier available from the existing auth layer.

### Coverage Gate Behavior

After a successful operator action, the coverage gate should observe the
updated state rather than the old stopped state. That means the next GitHub
issue intake scan can create or continue work instead of returning
`Covered { source: "workflow_runtime", state: "blocked" }`.

Do not make `blocked` or `failed` globally uncovered. They should remain
covered until the operator action changes state, because automatic redispatch
without operator intent would violate the conservative default.

### API And Dashboard Exposure

Add the structured reason fields to runtime tree nodes, operator monitor
`StuckWorkflow` rows, and `OperatorAction` rows. The serialized fields should
include `blocked_reason`, `unblock_hint`, `failure_reason`, `retry_hint`,
`last_stop`, and action eligibility booleans such as `can_unblock` and
`can_retry`. The React operator monitor should display the reason and a compact
action affordance only when the API reports the action is allowed.

The UI implementation should call the new runtime action routes and refresh the
operator monitor/runtime tree after success. It should not infer action
eligibility from free-text messages.

### Optional Recheck Policy

Automatic blocked recheck remains a later tranche. The first implementation may
define the configuration shape in the spec but should not implement polling
unless a follow-up PR explicitly covers it:

```yaml
blocked_recheck_interval_secs: 0
```

`0` means disabled. Any enabled recheck must re-read remote issue facts and
write explicit evidence before unblocking.

## Data Flow

Runtime job completion produces an `ActivityResult`. The reducer transitions
the workflow into `blocked` or `failed`, derives structured stop metadata, and
persists it in workflow instance `data` with a workflow event/decision. The
runtime tree and operator monitor read those fields. An authenticated operator
calls unblock or retry. The handler validates state, writes an audit event,
updates the instance to a dispatchable state, and returns the new state. The
next intake tick sees the updated state and can dispatch work normally.

## Alternatives Considered

- Make `blocked` and `failed` uncovered in `runtime_issue_state_is_covered`.
  Rejected because it would create automatic redispatch for workflows that may
  still be correctly waiting on humans or external repair.
- Delete workflow runtime rows to force redispatch. Rejected because direct
  table surgery is the problem this issue is removing.
- Add only dashboard buttons without structured state. Rejected because actions
  would still depend on free-text interpretation.
- Use path parameters for workflow ids in the first route. Rejected for the
  first implementation because workflow ids can include project paths and `/`;
  body-based routes match existing runtime merge/cancel behavior.
- Implement periodic blocked recheck immediately. Deferred because it changes
  conservative default behavior and needs its own evidence and configuration
  tests.

## Risks

- Security: recovery routes can re-dispatch agent work, so they must use the
  existing authenticated API middleware and must not bypass human gates.
- Compatibility: additive JSON fields are backward-compatible, but old rows may
  lack structured stop metadata.
- Logic: moving a workflow to the wrong state could duplicate work or skip
  planning. Implementation must use the workflow definition and targeted tests
  for each supported definition.
- Data integrity: operator actions must preserve historical stop evidence and
  record a fresh audit event.
- Operations: retrying non-retryable configuration failures can create loops.
  The first implementation should reject those failures instead of force
  retrying them.

## Test Plan

- [ ] Unit tests for stop-reason derivation from blocked and failed
      `ActivityResult` values.
- [ ] Runtime reducer tests proving blocked and failed transitions persist the
      structured metadata.
- [ ] Store/action tests proving unblock is accepted only from `blocked` and
      retry is accepted only from retryable `failed` workflows.
- [ ] Coverage-gate test proving the same issue is covered before unblock and
      not held by the old blocked state after unblock.
- [ ] HTTP route tests for success, not found, store unavailable, wrong state,
      and non-retryable failure responses.
- [ ] Operator monitor/runtime tree tests proving structured reason fields are
      serialized.
- [ ] Web tests for rendering reason text and invoking allowed actions.
- [ ] Usage guide docs update.
- [ ] Focused Rust tests: `cargo test -p harness-workflow runtime_failure` and
      targeted `cargo test -p harness-server` filters for the new routes and
      coverage gate.
- [ ] Web tests for the operator monitor when UI changes land.
- [ ] Final readiness for implementation PRs: `cargo fmt --all -- --check` and
      the relevant package tests.

## Rollback Plan

Disable or remove the unblock/retry routes and UI affordances, then revert the
metadata serialization changes. Because the metadata is additive JSON inside
workflow instance `data`, rollback does not require a database migration.
