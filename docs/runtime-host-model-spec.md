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
- `POST /api/runtime-hosts/{id}/runtime-jobs/{runtime_job_id}/lease/renew`
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
- `lifecycle: active | draining` (legacy records default to `active`)

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
3. `deregister` first persists `draining`, revokes every owned workflow runtime-job lease, releases pending task claims, and removes the host only after no workflow leases remain. A failed or interrupted cleanup leaves the host visibly draining and retryable.
4. Legacy task `claim` selects only tasks in `pending` status.
5. A non-expired lease blocks claims by other hosts.
6. Expired leases are reclaimable by any host.
7. Workflow runtime-job claims select only `runtime_kind = remote_host` jobs.
8. In-process runtime workers do not claim `remote_host` jobs.
9. Runtime-job completion requires the host id and exact lease expiration timestamp from the claim response. Upgraded clients also send the additive lease generation; legacy clients may omit it.
10. Runtime-job claim and renewal lease durations default to 60 seconds and accept only `1..=3600` seconds.
11. A draining host cannot claim or renew work. Heartbeat only updates host liveness and never extends a job lease.

## API Response Shape

`GET /api/runtime-hosts`
- `hosts: [{ id, display_name, capabilities, registered_at, last_heartbeat_at, online, lifecycle }]`
- `online` is computed as `now - last_heartbeat_at <= heartbeat_timeout_secs`

`POST /api/runtime-hosts/{id}/tasks/claim`
- success: `{ claimed: true, task_id, lease_expires_at }`
- none available: `{ claimed: false }`

`POST /api/runtime-hosts/{id}/runtime-jobs/claim`
- request: `{ lease_secs?: number }`
- success: `{ claimed: true, runtime_job_id, lease_generation, lease_expires_at, runtime_job }`
- none available: `{ claimed: false }`
- `runtime_job.input` carries the activity payload and runtime profile manifest needed by the external host.
- `lease_secs` is a target TTL from server time, defaults to `60`, and must be an integer in `1..=3600`; `null`, zero, and larger values are rejected.

`POST /api/runtime-hosts/{id}/runtime-jobs/{runtime_job_id}/lease/renew`
- request: `{ lease_generation, lease_expires_at, renewal_id, lease_secs?: number }`
- success: `{ renewed: true, runtime_job_id, lease_generation, lease_expires_at, replayed }`
- the server computes `max(current_expiry, server_now + lease_secs)`, so renewal never shortens a lease and never extends beyond the 3600-second server-time horizon.
- retry an ambiguous transport result with the same `renewal_id` and identical inputs; a live replay returns the original expiry with `replayed: true` and creates no second renewal event.
- stale, expired, revoked, reclaimed, wrong-host, wrong-generation, or draining ownership returns HTTP `409` with `{ error_code: "lease_lost", must_stop: true }`; the response never exposes another owner's identity or lease evidence.
- unknown host or job returns `404`, invalid input returns `400`, and unavailable durable storage returns `503`.

`POST /api/runtime-hosts/{id}/runtime-jobs/{runtime_job_id}/complete`
- request: `{ lease_expires_at, lease_generation?, result }`
- `result` is the workflow `ActivityResult` payload and may report `succeeded`, `failed`, `blocked`, or `cancelled`.
- success: `{ completed: true, runtime_job, workflow_event, decision }`
- stale or wrong lease: HTTP `409` with `{ completed: false, error }`

## Remote Executor Renewal Protocol

1. Schedule renewal at approximately one third of the confirmed TTL and keep at most one renewal request in flight per runtime job.
2. Treat heartbeat and per-job renewal as independent protocols. Heartbeat proves host liveness only; renewal of one job does not renew any other job.
3. On an ambiguous transport failure, retry only with the same `renewal_id`, generation, prior expiry, and duration.
4. On `404`, `409`, `must_stop: true`, or inability to confirm renewal before the last confirmed expiry, cancel local execution and suppress completion.
5. Deregistration is draining cleanup: do not claim or renew after draining begins, and retry deregistration until it succeeds or returns an operational failure.

## Rollback

Stop remote clients from calling renewal before disabling the renew route. The additive generation, lifecycle, and receipt data remain compatible with legacy claim and completion clients. Before removing draining/revocation behavior, verify that no host is draining and no runtime job depends on a renewed expiry.

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
