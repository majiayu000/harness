# Product Spec

## Linked Issue

GH-1461

## User Problem

Harness previously accumulated severe Postgres catalog growth before operators
had a visible signal. The orphan-schema reaper now bounds one known leak path,
but a stalled reaper or a new leak could again grow `pg_namespace`, `pg_class`,
and total database size without showing up in the health surface or operator
dashboard.

Operators need a cheap, low-frequency catalog census that makes catalog size
visible and loud when it crosses a configured threshold.

## Goals

- Add a Postgres catalog census for:
  - `pg_namespace` row count.
  - `pg_class` row count.
  - `pg_database_size(current_database())`.
- Expose the latest census in the existing `/health` response.
- Expose the same values in the usage monitor payload and dashboard UI.
- Emit an `error`-level log when catalog counts exceed a configurable threshold.
- Keep census cost bounded to catalog-only counts and avoid user-table scans.
- Test the threshold breach path with a lowered threshold.

## Non-Goals

- Running the one-time backlog cleanup for any operator database.
- Changing orphan-schema reaper logic, cadence, or ownership.
- Adding a new standalone observability service or metrics backend.
- Scanning user tables, task rows, runtime events, or application data.
- Blocking server startup solely because a catalog census query fails.

## User-Visible Behavior

When Harness is backed by Postgres, `/health` includes a `postgres_catalog`
object containing schema count, catalog object count, database size in bytes,
threshold metadata, sample time, and breach status. The usage monitor dashboard
shows the same values so an operator can see whether catalog growth is normal,
approaching the threshold, or already above threshold.

If the census cannot be collected because the database is unavailable or the
server is running without a Postgres-backed store, the response should report an
explicit unavailable state instead of fabricating zero counts.

When the configured threshold is breached, Harness logs an `error`-level line
with the sampled values and threshold source. The health response remains
available and includes the breach flag so automated checks can detect it.

## Acceptance Criteria

- [ ] `/health` includes schema count, catalog object count, database size, and
      sample metadata when Postgres is available.
- [ ] `/api/usage-monitor` includes the catalog census payload or an explicit
      unavailable state.
- [ ] The React usage monitor dashboard renders the three values and shows a
      warning/error state when threshold is breached.
- [ ] Thresholds are configurable, with safe defaults of schema count > 50,000
      and a startup-relative breach when the current schema count exceeds 3x the
      startup sample.
- [ ] Threshold breach emits an `error`-level log, not only a warning.
- [ ] Census queries only use catalog counts and `pg_database_size`; no
      user-table scans are introduced.
- [ ] Query frequency is bounded; normal dashboard polling does not issue
      unbounded catalog queries on every request.
- [ ] Unit or integration tests cover lowered-threshold breach behavior and the
      unavailable/no-Postgres state.

## Edge Cases

- The server starts without a Postgres-backed workflow/task store.
- The startup census succeeds but a later refresh fails.
- The startup census has zero schemas or is unavailable, so the 3x relative
  threshold cannot be computed.
- Absolute and relative thresholds disagree; a breach of either one should be
  loud.
- Multiple concurrent health/dashboard polls happen while a census refresh is
  already in progress.

## Rollout Notes

This is an observability change. It should be safe to ship behind the existing
server configuration surface with default thresholds. Operators can use the new
values to decide when to run existing cleanup tools, but this change must not
perform cleanup automatically.
