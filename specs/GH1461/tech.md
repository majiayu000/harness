# Tech Spec

## Linked Issue

GH-1461

## Product Spec

See `specs/GH1461/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| HTTP health | `crates/harness-server/src/http/misc_routes.rs` | `/health` returns task count, persistence degradation, runtime logs, circuit breakers, and isolation health. | This is the existing status endpoint named by GH-1461. |
| Health tests | `crates/harness-server/src/http/tests/health_route_tests.rs`, `route_helpers.rs` | Tests deserialize a fixed `HealthResponse` shape and cover degraded states. | The test helper must be extended when `/health` gains a catalog block. |
| Usage monitor API | `crates/harness-server/src/handlers/usage_monitor.rs` | `/api/usage-monitor` builds token, runtime, process, and diagnostic payloads. | The same catalog census should be exposed here for the dashboard. |
| Usage monitor UI | `web/src/types/usage_monitor.ts`, `web/src/routes/UsageMonitor.tsx` | React renders usage KPIs, active pressure, tables, and diagnostics from `/api/usage-monitor`. | The dashboard surface for catalog counts belongs here. |
| Postgres test pattern | `crates/harness-server/src/http/tests/*`, `crate::test_helpers::db_tests_enabled()` | DB-backed tests skip without local `HARNESS_DATABASE_URL`. | Catalog tests should follow existing DB skip/lock patterns or isolate query logic with fakes. |
| Orphan cleanup | `scripts/pg-orphan-cleanup.sh`, `crates/harness-server/src/http/orphan_reaper.rs` | Existing cleanup/reaper behavior is separate. | GH-1461 must observe catalog size, not change cleanup. |

## Proposed Design

1. Add a catalog census model in the server layer.
   - Define a serializable payload, for example `PostgresCatalogCensus`, with:
     - `state`: `available`, `unavailable`, or `error`.
     - `schema_count`.
     - `catalog_object_count`.
     - `database_size_bytes`.
     - `sampled_at`.
     - `startup_schema_count`.
     - `absolute_schema_threshold`.
     - `relative_schema_multiplier`.
     - `threshold_breached`.
     - `breach_reasons`.
     - sanitized `error` for unavailable/error states.
   - Keep field names snake_case to match existing Rust JSON payloads.
2. Add bounded census collection.
   - Query only:
     - `select count(*) from pg_catalog.pg_namespace`.
     - `select count(*) from pg_catalog.pg_class`.
     - `select pg_database_size(current_database())`.
   - Prefer one small helper module rather than embedding SQL in unrelated
     handlers.
   - Cache the latest sample in `AppState` or a dedicated state helper with a
     short minimum refresh interval so `/health` and dashboard polling do not
     run catalog queries every request.
   - Capture an initial startup sample when Postgres is available, or record an
     unavailable baseline when it is not.
3. Add threshold configuration.
   - Add server config fields for absolute schema threshold and relative
     multiplier if no equivalent config already exists.
   - Defaults:
     - absolute schema threshold: `50_000`.
     - relative schema multiplier: `3.0`.
   - Disable only with an explicit config value if the existing config style
     supports optionals; do not silently disable on invalid values.
4. Emit breach logging.
   - On refresh, if either threshold is breached, emit `tracing::error!` with
     schema count, catalog object count, database size, thresholds, and reasons.
   - Avoid logging raw connection strings or database errors that may include
     secrets.
5. Wire `/health`.
   - Add `postgres_catalog` to the `/health` JSON body.
   - Keep `/health` itself available when census is unavailable; the nested
     payload carries the error/unavailable state.
   - Treat a threshold breach as degraded unless implementation discovers an
     existing health policy that separates advisory health from degraded health.
     If not degraded, document the reason in the implementation PR.
6. Wire `/api/usage-monitor`.
   - Add the same catalog payload to `UsageMonitorResponse`.
   - Reuse the cached sample instead of running a second query path.
7. Wire the React dashboard.
   - Extend `web/src/types/usage_monitor.ts`.
   - Add a compact catalog KPI/panel in `web/src/routes/UsageMonitor.tsx`.
   - Show unavailable/error text when no sample is available.
   - Use existing formatting helpers for bytes and integers.

## Data Flow

The server collects or refreshes a catalog census from Postgres catalog tables,
stores the latest sanitized sample with threshold evaluation, and exposes that
sample through `/health` and `/api/usage-monitor`. The React usage monitor reads
the existing API payload and renders the counts. No cleanup or mutation is
performed.

## Alternatives Considered

- Run catalog counts on every dashboard poll. Rejected because GH-1461 requires
  bounded frequency.
- Store census history in a new table. Rejected for this issue; the requested
  behavior needs latest-value visibility and threshold logging, not retention.
- Put the values only in `/health`. Rejected because GH-1461 also requires the
  usage monitor dashboard to show them.
- Change orphan reaper cadence. Rejected because reaper behavior is a non-goal.

## Risks

- Performance: catalog counts over very large catalogs can still cost time.
  Mitigate with low frequency and no user-table scans.
- Reliability: health/dashboard handlers must not fail wholesale when the
  census query fails.
- Alert noise: repeated dashboard polls could emit repeated errors. Mitigate by
  logging on refresh decisions rather than every response serialization, and
  consider suppressing duplicate breach logs within the refresh interval.
- Config drift: threshold field names and defaults must match existing config
  conventions in `harness-core`.

## Test Plan

- [ ] Unit test threshold evaluation with absolute and startup-relative breaches.
- [ ] Unit test no-breach and unavailable baseline states.
- [ ] Handler test `/health` includes `postgres_catalog` and marks breach
      visibly with lowered thresholds.
- [ ] Usage monitor handler test includes the same catalog payload.
- [ ] Web test renders catalog counts and unavailable state in
      `web/src/routes/UsageMonitor` tests.
- [ ] Run focused Rust tests for health/usage monitor.
- [ ] Run focused web tests for usage monitor.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.

## Rollback Plan

Remove the catalog census helper/state, remove the `/health` and
`/api/usage-monitor` payload fields, remove the React panel/type additions, and
remove the new config fields. No data migration or cleanup rollback is required
because the implementation only reads Postgres catalog metadata.
