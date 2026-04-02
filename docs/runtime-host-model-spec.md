# Runtime Host Model (Issue #595)

## Goal

Add a minimal runtime host control surface so Harness can track external executors and safely lease pending tasks to them.

## Why

Harness is strong as a centralized control plane, but runtime host lifecycle is implicit today. We need explicit host registration and claim semantics before scaling execution beyond directly attached local agents.

## In Scope

1. In-memory runtime host registry with:
- host `register`
- host `deregister`
- host `heartbeat`
- host listing with `online` status derived from heartbeat freshness
2. In-memory task lease manager for pending tasks:
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

1. Persisting runtime hosts and leases across restart.
2. Remote executor callback APIs (`start/progress/complete/fail`).
3. Scheduler integration that auto-routes all tasks to runtime hosts.
4. Workspace/multi-tenant model migration.

## Data Model (V1)

`RuntimeHost`
- `id: String`
- `display_name: String`
- `capabilities: Vec<String>`
- `registered_at: DateTime<Utc>`
- `last_heartbeat_at: DateTime<Utc>`

`TaskLease`
- `task_id: TaskId`
- `host_id: String`
- `claimed_at: DateTime<Utc>`
- `expires_at: DateTime<Utc>`

## Behavior Rules

1. `register` is idempotent by `host_id` (upsert metadata and refresh heartbeat).
2. `heartbeat` on unknown host returns error.
3. `deregister` removes host and all leases owned by host.
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

1. In-memory leases can be lost on restart.
2. Claim fairness is best-effort because pending selection is linear scan.

## Verification

1. Unit tests for manager:
- duplicate claim blocked while lease active
- claim succeeds after lease expiry
- deregister clears leases
2. Handler tests:
- register + heartbeat + list returns online host
- claim endpoint returns expected payload and prevents double-claim
