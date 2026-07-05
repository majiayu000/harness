# Product Spec

## Linked Issue

GH-1456

## User Problem

Postgres-backed Harness tests still serialize large portions of the
`harness-server` suite behind process-global database locks. Even when each test
uses an isolated temporary data directory, helpers such as
`acquire_db_state_guard` and integration-local `DB_TEST_LOCK` values force tests
to run one at a time. This keeps the server test profile slow, masks accidental
shared-state coupling, and prevents safe adoption of `cargo-nextest`
process-level parallelism.

GH-1444 already made Postgres test setup safer by rejecting non-test databases
and cleaning up known temporary test schemas. GH-1456 finishes the next step:
database-backed tests must rely on per-test schema isolation instead of a global
database mutex.

## Goals

- Remove database-wide serialization from the normal `harness-server` and
  related Postgres test hot paths.
- Preserve the GH-1444 safety contract: tests must still refuse non-test
  databases and must not leak known temporary test schemas.
- Ensure each DB-backed test gets isolated storage through a unique data path or
  an explicit per-test schema guard.
- Keep process-global locks only for true process-global state such as `HOME`
  or environment-variable mutation.
- Make the local DB test script and CI verification compatible with default
  test parallelism and `cargo-nextest`.
- Record before/after timing evidence for the affected server DB profile.

## Non-Goals

- Changing production database resolution, store schema names, or runtime
  database routing.
- Weakening DB-backed assertions into no-op tests to gain speed.
- Removing `HOME_LOCK` from tests that intentionally mutate `HOME`.
- Reworking every large test module unrelated to DB isolation.
- Replacing all `cargo test` jobs in CI if a scoped nextest rollout is safer.

## User-Visible Behavior

When a developer runs the DB-backed test profile against a safe test database,
tests that use independent temporary data directories or explicit
`TestSchemaGuard` schemas run concurrently. A test that mutates only its own data
directory must not wait for unrelated DB-backed tests. A test that mutates
process-global state may still acquire the relevant process-global lock, but
that lock must be named for the global it protects and must not stand in for DB
isolation.

The `scripts/test-server-db.sh` helper must no longer force
`RUST_TEST_THREADS=1` for the whole profile. CI or local nextest configuration
must document and preserve the remaining intentionally serial cases while
letting DB-isolated tests run in parallel.

If a test attempts to use a non-test database URL, it keeps the GH-1444 behavior:
skip through the existing optional-DB path or fail before opening a pool, with
credentials redacted from diagnostics.

## Acceptance Criteria

- [ ] No DB-backed test helper depends on a process-global database mutex for
      correctness; `DB_STATE_LOCK` and integration-local DB-only locks are
      removed or made non-serializing compatibility shims.
- [ ] Tests that need shared Postgres state use unique data directories or
      explicit `TestSchemaGuard` schemas, not a global DB lock.
- [ ] Process-global locks remain only for true process-global state such as
      `HOME`, current environment variables, or other documented globals.
- [ ] `scripts/test-server-db.sh` runs the server DB profile with default
      parallelism instead of forcing `RUST_TEST_THREADS=1`.
- [ ] A `cargo-nextest` path is documented or configured for the DB-capable
      workspace profile, with intentionally serial tests called out explicitly.
- [ ] Focused tests prove that acquiring the legacy DB guard API no longer
      serializes unrelated work.
- [ ] Focused DB-backed test suites still pass with a safe
      `HARNESS_DATABASE_URL=.../harness_test`.
- [ ] Test database safety, schema cleanup, and leak cleanup behavior from
      GH-1444 remain covered.
- [ ] Before/after timing for the main server DB profile is recorded in the PR
      body or verification artifact.

## Edge Cases

- Tests that mutate `HOME` remain serialized by `HOME_LOCK`; they are not a DB
  isolation problem.
- Tests that intentionally verify shared-schema behavior may run inside the
  same schema, but that schema must be created with an explicit per-test guard.
- Tests that mutate process environment variables must keep the existing env
  lock or use an injected configuration path.
- If `cargo-nextest` is not installed locally, the implementation should still
  provide a deterministic `cargo test` fallback and document the nextest
  command for CI or maintainers.
- Panic cleanup must not require the process-wide DB mutex; owned test schemas
  still need best-effort cleanup through the GH-1444 guard.

## Rollout Notes

This is a test-infrastructure performance and reliability change. Developers
using a configured `HARNESS_DATABASE_URL` must continue using a test-designated
database such as `harness_test`. If any hidden shared-state coupling appears
after parallelism is restored, the fix is to give that test an explicit isolated
schema or a precise process-global lock, not to restore database-wide
serialization.
