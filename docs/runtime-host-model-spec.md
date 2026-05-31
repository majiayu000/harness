# Runtime Host Model (Issue #595)

## Goal

Add a runtime host control surface so Harness can track external executors and safely lease either legacy pending tasks or workflow runtime jobs to them.

## Why

Harness is strong as a centralized control plane, but runtime host lifecycle is implicit today. Runtime hosts need explicit registration and claim semantics before execution scales beyond directly attached local agents.

## In Scope

1. Runtime host registry with:
- host `register`
- host `deregister`
- host `heartbeat`
- host listing with `online` status derived from heartbeat freshness
2. Task leases stored on the authoritative task scheduler state:
- runtime host can claim one pending task
- duplicate claim prevention across hosts
- lease TTL support
3. Legacy task HTTP API endpoints:
- `GET /api/runtime-hosts`
- `POST /api/runtime-hosts/register`
- `POST /api/runtime-hosts/{id}/heartbeat`
- `POST /api/runtime-hosts/{id}/deregister`
- `POST /api/runtime-hosts/{id}/tasks/claim`
4. Workflow runtime job HTTP API endpoints:
- `POST /api/runtime-hosts/{id}/runtime-jobs/claim`
- `POST /api/runtime-hosts/{id}/runtime-jobs/{runtime_job_id}/complete`
5. Tests for lease correctness, heartbeat-driven status, runtime job claim ownership, and terminal callback ownership.

## Out of Scope

1. Progress streaming callbacks.
2. Scheduler integration that auto-routes all legacy tasks to runtime hosts.
3. Workspace/multi-tenant model migration.

## Data Model (V1)

`RuntimeHost`
- `id: String`
- `display_name: String`
- `capabilities: Vec<String>`
- `registered_at: DateTime<Utc>`
- `last_heartbeat_at: DateTime<Utc>`

`TaskScheduler` runtime-host lease fields
- `runtime_host_id: Option<String>`
- `lease_expires_at: Option<DateTime<Utc>>`

`RuntimeJob` workflow runtime lease fields
- `runtime_kind: RuntimeKind`
- `runtime_profile: String`
- `lease: Option<WorkflowLease>`
- `input: Value`
- `output: Option<Value>`

## Behavior Rules

1. `register` is idempotent by `host_id` (upsert metadata and refresh heartbeat).
2. `heartbeat` on unknown host returns error.
3. `deregister` removes host state and releases any pending task claims owned by that host.
4. Legacy task `claim` selects only tasks in `pending` status.
5. A non-expired lease blocks claims by other hosts.
6. Expired leases are reclaimable by any host.
7. Workflow runtime-job claims select only `runtime_kind = remote_host` jobs.
8. In-process runtime workers do not claim `remote_host` jobs.
9. Runtime-job completion requires the host id and exact lease expiration timestamp from the claim response.

## API Response Shape (V1)

`GET /api/runtime-hosts`
- `hosts: [{ id, display_name, capabilities, registered_at, last_heartbeat_at, online }]`
- `online` is computed as `now - last_heartbeat_at <= heartbeat_timeout_secs`

`POST /api/runtime-hosts/{id}/tasks/claim`
- success: `{ claimed: true, task_id, lease_expires_at }`
- none available: `{ claimed: false }`

`POST /api/runtime-hosts/{id}/runtime-jobs/claim`
- request: `{ lease_secs?: number }`
- success: `{ claimed: true, runtime_job_id, lease_expires_at, runtime_job }`
- none available: `{ claimed: false }`
- `runtime_job.input` carries the activity payload and runtime profile manifest needed by the external host.

`POST /api/runtime-hosts/{id}/runtime-jobs/{runtime_job_id}/complete`
- request: `{ lease_expires_at, result }`
- `result` is the workflow `ActivityResult` payload and may report `succeeded`, `failed`, `blocked`, or `cancelled`.
- success: `{ completed: true, runtime_job, workflow_event, decision }`
- stale or wrong lease: HTTP `409` with `{ completed: false, error }`

## Risks

1. Claim fairness is best-effort because pending selection is linear scan.
2. Host state is lightweight; legacy task lease truth is intentionally owned by the task store.
3. Workflow runtime-job lease truth is owned by `WorkflowRuntimeStore`.

## Verification

1. Unit tests for manager:
- registration upserts host metadata
- heartbeat updates online status
- stale heartbeat marks a host offline
2. Task-store and handler tests:
- register + heartbeat + list returns online host
- claim endpoint returns expected payload and prevents double-claim
- workflow runtime-job claim endpoint ignores local runtime jobs
- workflow runtime-job completion accepts terminal `ActivityResult` only when the exact lease is still owned by that host
