# Tech Spec

## Linked Issue

GH-1456

## Product Spec

See `specs/GH1456/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Server test helper | `crates/harness-server/src/test_helpers.rs` | `DB_STATE_LOCK` backs `acquire_db_state_guard()`, and `make_state_inner` holds it around construction of task, event, project, thread, and related stores. | This is the primary serialization point for many server tests. |
| DB safety helper | `crates/harness-core/src/db_test_safety.rs` | GH-1444 added safe test database URL validation, `TestSchemaGuard`, known test-schema cleanup, and leak cleanup planning. | This is the safety foundation that lets tests parallelize without touching production databases. |
| Server tests | `crates/harness-server/src/**/*tests*.rs`, `crates/harness-server/tests/*.rs` | Many tests still call `acquire_db_state_guard()` or local `DB_TEST_LOCK` helpers before building isolated app state. Many also correctly use `HOME_LOCK` for `HOME` mutation. | DB-only locks should be removed or made non-serializing; real process-global locks should remain. |
| Core and observe tests | `crates/harness-core/src/*tests.rs`, `crates/harness-observe/src/**/*tests.rs` | Core uses a small env lock for environment mutation. Observe/server shared-schema tests now use `TestSchemaGuard` for named temporary schemas. | These patterns distinguish real process-global serialization from unnecessary DB serialization. |
| Local script | `scripts/test-server-db.sh` | Forces `RUST_TEST_THREADS=1` for `harness-server` DB tests. | This hides shared-state bugs and prevents measuring the parallelized profile. |
| CI | `.github/workflows/ci.yml` | Runs `cargo test` with `HARNESS_DATABASE_URL=.../harness_test`; no nextest profile is configured yet. | CI already uses a safe test database, so it can run the same isolated profile once remaining serial assumptions are removed. |

## Proposed Design

1. Replace the process-global DB mutex with a compatibility guard that does not
   serialize unrelated tests.
   - Remove `DB_STATE_LOCK` from `test_helpers.rs`.
   - Keep `acquire_db_state_guard()` only as a short-term API compatibility shim
     if removing every call site would require unrelated large-file churn.
   - The compatibility guard must be a no-op value and must not hold a mutex.
   - Add a focused test proving two guard acquisitions can overlap.
2. Ensure state construction relies on per-test isolation.
   - `make_state_inner` should use the caller's temporary data directory and
     the GH-1444 safe database URL; it should not hold a database-wide guard.
   - Tests that need shared-schema assertions must keep using
     `TestSchemaGuard` with known prefixes.
   - Tests that need process-global `HOME` changes should keep `HOME_LOCK` and
     `HomeGuard`.
3. Remove or neutralize local DB-only locks in integration tests.
   - Replace local `DB_TEST_LOCK` helpers that only protect DB state with no-op
     guards or direct per-test isolation.
   - Keep locks only when the test mutates process environment variables,
     process-wide configuration, global singleton state, or `HOME`.
   - Name any remaining lock after the global it protects.
4. Restore parallel DB test execution.
   - Update `scripts/test-server-db.sh` to stop forcing
     `RUST_TEST_THREADS=1`.
   - Add a nextest configuration or documented command for the DB-capable
     workspace profile. If nextest is not installed locally, keep `cargo test`
     as the deterministic fallback.
   - Call out intentionally serial test groups in config/docs rather than
     serializing the whole server DB profile.
5. Add leak and regression checks.
   - Keep GH-1444 tests for safe URL validation and known temporary schema
     cleanup.
   - Add a focused server helper test that exercises concurrent DB guard
     acquisition without waiting for another test's critical section.
   - Run the focused server DB suites with `HARNESS_DATABASE_URL` pointing at a
     safe test database and record elapsed-time evidence.

## Data Flow

Test setup resolves the candidate database URL through
`resolve_test_database_url`, which validates the database name before any pool
opens. Store constructors then derive path-based schemas from each test's
temporary data directory, or receive an explicit schema from `TestSchemaGuard`
for shared-schema tests. No process-global database mutex participates in this
flow.

Tests that need process-global state still acquire the corresponding global
lock before mutation. That lock protects process state, not the database.

## Alternatives Considered

- Keep `DB_STATE_LOCK` and rely on nextest process isolation later. Rejected
  because the in-process `cargo test` profile remains slow and the lock hides
  missing per-test isolation.
- Delete all `acquire_db_state_guard()` call sites in one large PR. Rejected as
  unnecessary churn if the API can be made non-serializing first; follow-up
  cleanup can remove dead calls module by module.
- Make every DB-backed test create a named schema manually. Rejected because
  path-derived schemas already provide isolation for tests with unique data
  directories, while `TestSchemaGuard` is enough for explicit shared-schema
  cases.
- Remove `HOME_LOCK` while touching tests. Rejected because it protects real
  process-global state and is outside the DB parallelization problem.

## Risks

- Security: database URL diagnostics must continue to redact credentials, and
  schema cleanup must keep validating identifiers before interpolation.
- Compatibility: tests with hidden shared data directories may start racing once
  the DB lock is removed. Fix those tests by giving them unique paths or
  explicit schemas.
- Performance: opening multiple Postgres pools concurrently can increase
  connection pressure. Existing test pool defaults should stay bounded, and CI
  Postgres should be monitored during the rollout.
- Maintenance: a no-op compatibility guard can become confusing if it remains
  forever. The implementation should mark it as compatibility-only and prefer
  removing new call sites.

## Test Plan

- [ ] Unit tests: the server DB guard compatibility API does not serialize
      overlapping acquisitions.
- [ ] Unit tests: `db_tests_enabled()` and `test_database_url()` retain GH-1444
      safe database URL behavior.
- [ ] Integration tests: server shared-schema and backfill tests pass against
      `HARNESS_DATABASE_URL=.../harness_test`.
- [ ] Integration tests: representative server route/state tests that used the
      old DB guard pass without DB serialization.
- [ ] Script verification: `scripts/test-server-db.sh --help` or dry-run output
      no longer advertises forced `RUST_TEST_THREADS=1`.
- [ ] Nextest verification: `cargo nextest run --workspace` or the documented
      DB-capable nextest subset passes when the tool is installed.
- [ ] Fallback verification: `cargo test -p harness-server --lib` and the
      relevant integration tests pass with a safe test database.
- [ ] Repository checks: `cargo fmt --all -- --check`,
      `cargo check --workspace --all-targets`,
      `cargo clippy --workspace --all-targets -- -D warnings`, and SpecRail
      workflow checks pass.

## Rollback Plan

Revert the test helper, integration-test lock, script, and nextest/CI changes.
Because this only changes test infrastructure, no production database migration
or persisted data rollback is required. If a specific test proves non-isolated,
prefer a targeted per-test schema or precise process-global lock over restoring
the database-wide mutex.
