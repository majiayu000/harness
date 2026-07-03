# Product Spec

## Linked Issue

GH-1436

## User Problem

Operators running harness against Postgres accumulate tens of thousands of leaked per-workspace schemas (16,933 `h<hex>` schemas, 5.7 GB observed 2026-07-03). The automatic orphan reaper added in #1216/#1220 never touches them because they predate schema registration (#1213), so catalog bloat — the dominant live performance drag (sqlx slow-statement warnings on runtime queries) — keeps growing on any deployment older than the registry.

## Goals

- The running server automatically reclaims legacy (unregistered) orphaned path-derived schemas, not only registered ones.
- Reaper progress and coverage are observable: each tick reports registered and legacy scan counts.
- Reaping remains conservative: a schema whose owner cannot be proven dead is never dropped.

## Non-Goals

- Dropping non-path-derived schemas (workflow runtime tables, registry schema itself).
- Changing the workspace-to-schema naming scheme.
- Backfilling ownership metadata for schemas that will be dropped anyway.

## Behavior Invariants

1. A legacy `h<hex>` schema with no registry row and no owning workspace directory on the host is reaped within a bounded number of reaper ticks.
2. A schema whose owning workspace directory exists is never dropped, registered or not.
3. A schema that cannot be safely attributed (ambiguous owner, transient IO error during check) is kept and reported, never dropped.
4. Each reaper tick logs scanned/reaped counts split by class (registered vs legacy) at info level; tick failures are logged at warn or higher.
5. The reaper honors `storage.orphan_reaper_enabled` and `storage.orphan_reaper_interval_secs` for both classes; ticks recur at the configured interval for the lifetime of the server process.
6. Legacy reaping is rate-limited per tick (bounded batch) so a first run against a 17k-schema backlog cannot starve the database.

## Acceptance Criteria

- [ ] On a database seeded with unregistered orphaned `h<hex>` schemas, the running server reduces their count to zero over successive ticks without operator action.
- [ ] Live schemas (owner directory present) survive all ticks, registered or not.
- [ ] Tick log line includes both class counts; production DB namespace count trends down after deploy.
- [ ] `scripts/pg-orphan-cleanup.sh` documents the legacy class and matches server behavior.

## Edge Cases

- Registry table absent (fresh or partially migrated DB): legacy scan still works.
- Schema name matches `h<hex>` but is not exactly the expected hash length — must not be dropped blindly; only names matching the canonical pattern are candidates.
- Reaper running on a host that does not own the workspace paths (owner dirs unreachable but not deleted): invariant 3 applies — keep.
- Interrupted reap mid-batch: next tick resumes without double-drop errors.

## Rollout Notes

First deploy against the production DB will reap ~17k schemas over multiple ticks; expect elevated catalog churn during the drain. Recommend snapshotting the DB (or running the CLI dry-run first) before enabling. No config migration: existing keys gate the new behavior.
