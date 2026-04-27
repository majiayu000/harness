# Runtime Host Model (Issue #595)

## Goal

Add a minimal runtime host control surface so Harness can track external executors and safely lease pending tasks to them.

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
3. HTTP API endpoints:
- `GET /api/runtime-hosts`
- `POST /api/runtime-hosts/register`
- `POST /api/runtime-hosts/{id}/heartbeat`
- `POST /api/runtime-hosts/{id}/deregister`
- `POST /api/runtime-hosts/{id}/tasks/claim`
4. Tests for lease correctness and heartbeat-driven status.

## Out of Scope

1. Remote executor callback APIs (`start/progress/complete/fail`).
2. Scheduler integration that auto-routes all tasks to runtime hosts.
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

## Behavior Rules

1. `register` is idempotent by `host_id` (upsert metadata and refresh heartbeat).
2. `heartbeat` on unknown host returns error.
3. `deregister` removes host state and releases any pending task claims owned by that host.
4. `claim` selects only tasks in `pending` status.
5. A non-expired lease blocks claims by other hosts.
6. Expired leases are reclaimable by any host.

## API Response Shape (V1)

`GET /api/runtime-hosts`
- `hosts: [{ id, display_name, capabilities, registered_at, last_heartbeat_at, online }]`
- `online` is computed as `now - last_heartbeat_at <= heartbeat_timeout_secs`

`POST /api/runtime-hosts/{id}/tasks/claim`
- success: `{ claimed: true, task_id, lease_expires_at }`
- none available: `{ claimed: false }`

## Risks

1. Claim fairness is best-effort because pending selection is linear scan.
2. Host state is lightweight; task lease truth is intentionally owned by the task store.

## Verification

1. Unit tests for manager:
- registration upserts host metadata
- heartbeat updates online status
- stale heartbeat marks a host offline
2. Task-store and handler tests:
- register + heartbeat + list returns online host
- claim endpoint returns expected payload and prevents double-claim
