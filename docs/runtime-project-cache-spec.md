# Runtime Project Cache (Issue #594)

## Goal

Add a watched project cache for long-lived runtime hosts so project context can be warm-synced and reused across claims.

## Why

Runtime hosts should not start from a cold project view on every cycle. We need an explicit host-scoped cache of watched projects, with a sync API and predictable lifecycle.

## In Scope

1. Host project cache manager with runtime-state persistence.
2. Host-scoped project sync endpoint.
3. Host-scoped project list endpoint.
4. Registry/path resolution for project tokens during sync.
5. Cache cleanup when runtime host deregisters.
6. API tests for path sync, project-id sync, and unknown host handling.

## Out of Scope

1. Background file watcher or git index prefetch.
2. Automatic scheduler routing based on watched-project affinity.
3. Multi-tenant workspace model migration.

## API (V1)

`POST /api/runtime-hosts/{host_id}/projects/sync`
- request: `{ "projects": [{ "project": "<id-or-path>" }] }`
- behavior:
- if token is an existing directory, use canonical path
- else resolve as registered project ID
- enforce `allowed_project_roots` and project-root validation
- response:
- `{ host_id, last_synced_at, project_count, projects[] }`

`GET /api/runtime-hosts/{host_id}/projects`
- response:
- same snapshot shape as sync response

## Data Model

`WatchedProjectInput`
- `project_id: Option<String>`
- `root: String`

`HostProjectCacheSnapshot`
- `host_id: String`
- `last_synced_at: String`
- `project_count: usize`
- `projects: [{ project_id, root, synced_at }]`

## Rules

1. Sync is full replacement for that host cache (latest snapshot wins).
2. Project roots are deduplicated by canonical root path.
3. Unknown runtime host returns `404`.
4. Deregistered host clears cached projects.

## Risks

1. Runtime-state persistence is best-effort; a persist failure is surfaced by the handler and retried by later mutations.
2. Sync does not currently warm git metadata; only project identity/root is cached.

## Verification

1. Sync by project path then list returns cached canonical root.
2. Sync by project ID resolves registry root and stores `project_id`.
3. Sync on unknown host returns `404`.
