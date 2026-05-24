# Runtime Submission Identity Contract

Issue: #1127

This document defines the identity model for runtime-owned submissions and the
compatibility rules that must stay in place while Harness migrates away from
legacy task-row semantics.

## Terms

### `workflow_id`

`workflow_id` is the workflow runtime instance id. It identifies the state
machine row in `WorkflowRuntimeStore` and is the authoritative handle for
workflow-runtime operations:

- runtime cancellation
- runtime merge or approve actions
- runtime tree inspection
- workflow event, decision, command, and job correlation

`workflow_id` is not a legacy task row id and must not be used as a `/tasks/{id}`
path parameter unless an endpoint explicitly documents workflow-id addressing.

### `submission_id`

`submission_id` is the long-term public submission handle. It identifies the
operator-facing submission across task-compatible routes such as detail, stream,
proof, artifacts, and prompts.

During the migration, existing runtime workflows may not have a stored
`submission_id`. For those rows, the compatibility submission handle is:

1. the first string in `data.task_ids`, when present
2. otherwise `data.task_id`

That rule preserves links created before explicit `submission_id` support while
still allowing newer `data.task_id` values to be recorded as retry or
resubmission correlation handles.

### `task_id`

`task_id` has two meanings today:

- for legacy task-runner rows, it is the real `TaskStore` row id
- for runtime-owned submissions, it is a compatibility alias for the submission
  handle

New runtime-owned API contracts must treat `task_id` as an alias only. The
presence of `task_id` in a runtime response must not imply that a legacy
`TaskStore` row exists.

### Runtime Workspace Task Id

Runtime worktree leases may use synthetic workspace ids such as
`runtime-wf-...`. Those ids are workspace lease identifiers, not public
submission handles. Worktree cards must carry `workflow_id` separately when a
runtime workflow owns the lease.

## API Contract

### `POST /tasks`

Runtime-owned issue and prompt submissions return:

- `execution_path: "workflow_runtime"`
- `workflow_id`
- `task_id` as the current compatibility alias

After #1128, runtime-owned submissions should also return `submission_id`.
During the migration, `submission_id` and `task_id` must be equal for runtime
responses.

Legacy task-runner submissions may keep returning only `task_id` when no
runtime workflow exists.

### `GET /tasks`

Runtime-owned rows returned by the list endpoint must expose enough identity for
all dashboard actions:

- `workflow_id` for workflow-runtime actions
- `submission_id` once #1128 lands
- `task_id` as a temporary compatibility alias
- `execution_path: "workflow_runtime"`

Legacy task rows keep `task_id` as the primary id and omit `workflow_id`.

### `GET /tasks/{id}`

The `{id}` parameter addresses a submission handle during migration:

- runtime-owned rows resolve by `submission_id` or any historical `task_id`
  alias recorded in `data.task_ids`
- legacy rows resolve by real `TaskStore` task id

The response for a runtime-owned row must include `workflow_id` so clients do
not need to infer runtime ownership from the path id.

### `GET /tasks/{id}/stream`

The stream route accepts the same submission handle as `GET /tasks/{id}`.
Runtime-owned streams are resolved through workflow-runtime metadata and events.

### `POST /tasks/{id}/cancel`

This route remains a compatibility endpoint during migration. It may accept a
runtime submission handle, but clients that have `workflow_id` must prefer:

```text
POST /api/workflows/runtime/cancel
```

with `workflow_id` in the request body.

### `GET /tasks/{id}/proof`

The proof route accepts the submission handle. Runtime-owned proof responses
must include `workflow_id` as a quality signal or explicit field so proof
consumers can trace the underlying workflow.

### `GET /tasks/{id}/artifacts`

The artifacts route accepts the submission handle. Runtime-owned artifact lookup
must not require a legacy `TaskStore` row once #1128 lands.

### `GET /tasks/{id}/prompts`

The prompts route accepts the submission handle. Runtime-owned prompt lookup
must not require a legacy `TaskStore` row once #1128 lands.

## Migration Phases

### Phase 0: Current Compatibility

- Runtime workflows persist `data.task_id` and `data.task_ids`.
- The public runtime submission handle is the compatibility handle derived from
  `data.task_ids[0]` or `data.task_id`.
- Historical handles in `data.task_ids` remain lookup aliases.
- `task_id` in runtime API responses is an alias, not proof of a legacy row.

### Phase 1: Add Explicit `submission_id`

- Runtime workflow data gains `data.submission_id`.
- New runtime workflow rows write `submission_id` at creation.
- Existing rows derive `submission_id` from the Phase 0 compatibility handle.
- Runtime responses include both `submission_id` and `task_id`.
- `task_id` remains equal to `submission_id` for runtime-owned responses.

### Phase 2: Prefer Runtime-Native Handles

- Dashboard and SDK flows store `submission_id` for task-compatible routes.
- Dashboard and SDK flows store `workflow_id` for runtime workflow actions.
- Runtime-owned detail, stream, cancel, proof, artifacts, and prompts work
  without a legacy task row.
- Legacy task rows continue to use real `task_id` values.

### Phase 3: Remove Legacy Compatibility

- Remove runtime-owned reliance on `data.task_id` as the public handle.
- Keep historical alias lookup only for persisted workflows that predate
  `submission_id`, or remove it with an explicit migration.
- Remove `/tasks` compatibility branches that only exist to make runtime-owned
  workflows look like legacy task rows.

## Route Ownership Rules

- Use `workflow_id` for workflow-runtime actions.
- Use `submission_id` for task-compatible read and stream routes.
- Use `task_id` only for real legacy task rows or as a temporary response alias.
- Do not use runtime workspace ids as submission handles.
- Do not infer runtime ownership from id shape. Use `execution_path` or
  `workflow_id`.

## Test Contract

Contract tests must lock these invariants before #1128 and #1129 change the
implementation:

- runtime submissions preserve historical task handles for lookup
- the Phase 0 compatibility handle is stable even if `data.task_id` changes on a
  later resubmission
- runtime create, list, detail, stream, cancel, proof, artifact, and prompt
  routes document their identity ownership
- runtime-owned responses expose `workflow_id`
- runtime-owned flows do not require a legacy `TaskStore` row
