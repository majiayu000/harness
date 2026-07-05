# Product Spec

## Linked Issue

GH-1444

## User Problem

Postgres-backed test suites can resolve the same database URL used by a local or
production Harness instance. When those tests create per-run schemas such as
`workspace_lease_scope_test_*`, `event_store_jsonl_test_*`,
`event_store_backfill_test_*`, and `event_store_scope_test_*`, interrupted or
panicking runs can leave the schemas behind. The existing schema reaper focuses
on path-derived `h<hex>` schemas and does not remove these named test schemas.

This makes `cargo test --workspace` unsafe as a readiness gate for later
kernel-contraction work: running verification can mutate and pollute the
operator's primary database.

## Goals

- Prevent Postgres-backed tests from running against a non-test-designated
  database by default.
- Keep test setup fail-closed: an unsafe configured database URL should skip or
  fail test DB setup before creating schemas.
- Drop temporary test schemas created by focused Postgres test helpers on normal
  completion and panic unwind.
- Provide a dry-run-first cleanup path for existing leaked `*_test_*` schemas.
- Preserve CI behavior where `HARNESS_DATABASE_URL` already points at
  `harness_test`.
- Keep production server and CLI database resolution unchanged outside test
  helpers and cleanup tooling.

## Non-Goals

- Solving full per-test parallel schema isolation; that is tracked by GH-1456.
- Dropping path-derived `h<hex>` schemas; that remains owned by the existing
  path-derived schema cleanup tools.
- Changing production store schema names, migrations, or runtime database
  routing.
- Automatically deleting schemas without an explicit apply/confirm flag.
- Requiring Docker for all local tests.

## User-Visible Behavior

When a Postgres-backed test helper resolves `HARNESS_DATABASE_URL` or the
repository config fallback, it must confirm that the target database is marked
as safe for tests before opening a pool that can create schemas. A database is
safe when its database name is clearly test-designated, such as `harness_test`
or a name ending in `_test`, or when an explicit escape hatch is set for
maintainers who intentionally run tests against another disposable database.

If the resolved URL is absent or points at a non-test-designated database, tests
that already skip when no database is available continue to skip. Test helpers
that require a configured Postgres database fail with a direct error before any
schema is created. The error names the safety rule and the escape hatch without
printing credentials.

For test helpers that create ad hoc schemas matching `*_test_*`, a cleanup guard
drops those schemas on normal completion and during panic unwind. Cleanup errors
must not be silently swallowed in tests that can surface them; where `Drop`
cannot return an error, the guard records the failure for explicit assertions or
logs it with enough schema context for diagnosis.

Operators also get a cleanup command or script that lists leaked test schemas in
dry-run mode first. Applying cleanup requires an explicit flag and only targets
schema names matching the bounded test-schema patterns used by this repository.

## Acceptance Criteria

- [ ] Test DB helpers reject or skip non-test-designated database URLs before
      opening pools that can create schemas.
- [ ] CI with `HARNESS_DATABASE_URL=.../harness_test` continues to run
      Postgres-backed tests without extra configuration.
- [ ] Maintainers have an explicit documented escape hatch for disposable
      non-`*_test` database names.
- [ ] Temporary named test schemas are dropped by an RAII-style guard on normal
      completion and panic unwind.
- [ ] A dry-run cleanup command reports existing leaked `*_test_*` schemas
      without dropping them by default.
- [ ] Applying cleanup requires an explicit flag and drops only known
      repository test-schema patterns.
- [ ] Tests cover safe URL detection, unsafe URL rejection, secret redaction in
      database URL error text, cleanup planning, and guard cleanup behavior.
- [ ] GH-1434 verification guidance can rely on workspace tests without
      polluting the primary database.

## Edge Cases

- A URL such as `postgres://user:pass@localhost:5432/harness` is unsafe by
  default even on localhost.
- A URL such as `postgres://user:pass@localhost:5432/harness_test` is safe.
- A URL whose database name is percent-encoded must be decoded or compared in a
  way that cannot misclassify `harness` as safe.
- Query parameters, passwords, and usernames must not appear in safety error
  messages.
- Cleanup must ignore schemas that merely contain `test` in the middle of an
  unrelated name and must ignore path-derived `h<hex>` schemas.
- If a cleanup guard cannot drop a schema because the database is already gone,
  the failure should be visible to the test owner but must not trigger cleanup
  of unrelated schemas.

## Rollout Notes

This is a test-infrastructure safety fix. Developers whose local config points
at the primary Harness database will see Postgres-backed tests skip or fail
before mutation until they set `HARNESS_DATABASE_URL` to a test database such as
`postgres://harness:harness@localhost:5432/harness_test`. CI already uses a
test-designated database name.
