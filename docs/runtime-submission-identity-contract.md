# Runtime Submission Identity Contract

Issue: #1127

This document defines the identity model after removal of the legacy task HTTP
compatibility layer. Workflow-runtime state is authoritative for every public
submission route.

## Terms

### `workflow_id`

`workflow_id` identifies the state-machine instance in
`WorkflowRuntimeStore`. Use it for workflow actions and correlation:

- cancel, merge, unblock, and retry
- runtime tree inspection
- workflow event, decision, command, and job correlation

Do not use a `workflow_id` as a submission path parameter unless a route
explicitly documents workflow-id addressing.

### `submission_id`

`submission_id` is the public handle for an operator-facing submission. Use it
with list-derived links for detail, stream, proof, artifacts, and prompts.

Historical runtime rows created before explicit submission ids may derive the
handle from the first string in `data.task_ids`, or from `data.task_id` when the
array is absent. That lookup rule preserves old runtime links; it does not imply
that a legacy task row exists.

### `task_id`

`task_id` is a deprecated response alias for `submission_id`. Runtime responses
keep the alias for serialized-client compatibility, but callers must not infer
task-store ownership from it. New clients should persist `submission_id` and
`workflow_id` instead.

### `request_id`

`request_id` identifies the accepted intake request. It may differ from the
stable public submission handle after a retry or resubmission. It is a
correlation value, not a detail-route handle.

### Runtime workspace task id

Runtime worktree leases may use synthetic workspace ids such as
`runtime-wf-...`. Those ids identify workspace leases only. They are neither
submission handles nor workflow ids.

## HTTP Contract

### Create and list

`POST /api/workflows/runtime/submissions` returns an accepted runtime
submission with:

- `execution_path: "workflow_runtime"`
- `workflow_id`
- `submission_id`
- `task_id`, equal to `submission_id`, as a deprecated alias
- `request_id`

`GET /api/workflows/runtime/submissions` lists only workflow-runtime
projections. Every row exposes the public submission handle and the underlying
workflow identity needed by dashboard actions.

### Detail and evidence

The following routes address `{id}` as a submission handle:

- `GET /api/workflows/runtime/submissions/{id}`
- `GET /api/workflows/runtime/submissions/{id}/stream`
- `GET /api/workflows/runtime/submissions/{id}/proof`
- `GET /api/workflows/runtime/submissions/{id}/artifacts`
- `GET /api/workflows/runtime/submissions/{id}/prompts`

Rows with an explicit `submission_id` resolve through that value. Historical
runtime rows may continue to resolve through the pre-existing alias derivation
described above. All data comes from workflow-runtime state and must not require
a legacy `TaskStore` row.

### Workflow actions

Use `workflow_id`, not a submission handle, with these authenticated actions:

- `POST /api/workflows/runtime/cancel`
- `POST /api/workflows/runtime/merge`
- `POST /api/workflows/runtime/unblock`
- `POST /api/workflows/runtime/retry`

Live approval responses use the turn and request ids supplied by runtime events:

`POST /api/workflows/runtime/turns/{turn_id}/approvals/{request_id}`.

## Removed Compatibility

The `/tasks` create, batch, list, detail, stream, proof, artifact, prompt,
cancel, and merge routes have been removed. Callers must not construct those
paths from `task_id`. Batch callers submit each item separately to the runtime
submission endpoint so every request receives its own durable workflow result.

## Route Ownership Rules

- Use `workflow_id` for workflow-runtime actions.
- Use `submission_id` for submission read and stream routes.
- Treat `task_id` as a deprecated serialized alias only.
- Treat `request_id` as intake correlation only.
- Do not use runtime workspace ids as public handles.
- Do not infer ownership from id shape; use explicit response fields.

## Test Contract

Contract tests lock these invariants:

- new submissions persist and return stable `submission_id` values
- `task_id` equals `submission_id` in runtime responses
- retries do not change the public submission handle
- historical runtime aliases remain resolvable
- create, list, detail, stream, proof, artifact, and prompt routes do not depend
  on a legacy task row
- workflow actions require `workflow_id`
- missing workflow-runtime persistence fails closed rather than returning
  partial legacy data
