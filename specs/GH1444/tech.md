# Tech Spec

## Linked Issue

GH-1444

## Product Spec

See `specs/GH1444/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Database URL resolution | `crates/harness-core/src/db_pg.rs` | `resolve_database_url` reads explicit config, `HARNESS_DATABASE_URL`, user config, and repository defaults. It is used by production code and tests. | Production behavior must remain unchanged, but tests need an additional safety gate. |
| Core DB tests | `crates/harness-core/src/db_tests.rs` | Tests open path-derived schemas through `Db::open_legacy_path_schema` and helper pools. They do not currently guard the target database name. | These tests can create schemas in any resolved database. |
| Server test helpers | `crates/harness-server/src/test_helpers.rs` | `db_tests_enabled` and `test_database_url` use `resolve_database_url(None)` and open pools directly. | This is the main reusable gate for server route and store tests. |
| Observe event-store tests | `crates/harness-observe/src/event_store/*_tests.rs` | Shared-schema tests create named `*_test_*` schemas and close pools manually. | These schemas match the leaked production catalog evidence. |
| Workflow/server store tests | `crates/harness-workflow`, `crates/harness-server` | Several tests perform local `resolve_database_url(None)` checks and skip if no DB exists. | The safety helper needs to be shared enough to avoid per-test drift. |
| Existing cleanup | `crates/harness-cli/src/bin/harness-pg-schema-cleanup.rs`, `scripts/pg-orphan-cleanup.sh` | Cleans registered/path-derived schemas and optionally legacy `h<hex>` schemas. | GH-1444 needs a separate bounded plan for repository test-schema names. |
| CI | `.github/workflows/ci.yml` | Uses `HARNESS_DATABASE_URL=postgres://postgres:postgres@localhost:5432/harness_test` for DB jobs. | The new safety gate must permit current CI. |

## Proposed Design

1. Add a test-only database safety helper in `harness-core`.
   - Place the reusable parsing and redaction code in a normal module so
     multiple crates can use it in tests and cleanup tooling.
   - Provide `is_test_database_url(url: &str) -> bool` and
     `validate_test_database_url(url: &str) -> anyhow::Result<()>`.
   - Treat a URL as safe when the database name is exactly `harness_test`, ends
     with `_test`, starts with `test_`, or when
     `HARNESS_ALLOW_NON_TEST_DATABASE_FOR_TESTS=1` is set.
   - Redact usernames, passwords, query strings, and fragments in errors.
2. Route Postgres-backed test setup through the safety helper.
   - In `harness-server::test_helpers`, make `db_tests_enabled` return `false`
     for unsafe URLs and make `test_database_url()` error before returning an
     unsafe URL.
   - In `harness-core` DB tests, check the safety helper before opening a pool.
   - In `harness-observe` shared-schema/store tests and workflow/server tests
     that resolve database URLs directly, validate the URL before opening a
     pool or use the common helper when available.
3. Add an RAII schema cleanup helper for named test schemas.
   - Introduce a `TestSchemaGuard` helper for tests that create named schemas
     with `unique_test_schema`.
   - The guard owns the schema name and setup pool/database URL and drops the
     schema with `DROP SCHEMA IF EXISTS <validated_identifier> CASCADE`.
   - Schema names must pass the existing identifier validation before being
     interpolated into SQL.
   - Tests should call an explicit async cleanup method where possible; `Drop`
     should perform best-effort cleanup only when a runtime is still available
     and record/log failures.
4. Add bounded leaked-test-schema cleanup tooling.
   - Extend `harness-pg-schema-cleanup` with `--test-schemas` dry-run mode and
     `--apply --test-schema <name>` or `--apply --drop-test-schemas` for
     explicit deletion.
   - Match only known repository prefixes:
     `workspace_lease_scope_test_%`, `event_store_jsonl_test_%`,
     `event_store_backfill_test_%`, and `event_store_scope_test_%`.
   - Print counts and schema names in dry-run mode. Do not drop by default.
   - Reuse identifier validation and parameterized catalog queries.
5. Document the local test database contract.
   - Update `scripts/test-server-db.sh` usage text and docs that mention
     `HARNESS_DATABASE_URL` to use a `harness_test` database.
   - Document the escape hatch and make clear it is only for disposable
     non-production databases.

## Data Flow

Test setup resolves the candidate database URL exactly as before. Before opening
a Postgres pool, test helpers call the test database safety helper. Unsafe URLs
produce a redacted diagnostic and no pool is opened. Safe URLs proceed into the
existing store constructors and migrations.

Tests that need named schemas request a `TestSchemaGuard` before passing the
schema name into `PgStoreContext::from_schema`. The guard validates the schema
identifier, records ownership for cleanup, and drops the schema at the end of
the test. Store code remains unaware of the guard.

Cleanup tooling connects to the operator-selected database URL, queries
`pg_namespace` for known test-schema prefixes, prints a dry-run plan, and only
drops validated names when explicit apply flags are provided.

## Alternatives Considered

- Require Docker for every DB test. Rejected because CI already supplies
  Postgres and local developers may use any disposable Postgres instance.
- Change `resolve_database_url` globally to reject non-test names during tests.
  Rejected because production code and command tests may legitimately exercise
  URL resolution without opening a DB or creating schemas.
- Drop every schema matching `%test%`. Rejected as too broad and unsafe.
- Leave cleanup to manual SQL. Rejected because GH-1444 needs a repeatable,
  dry-run-first workflow that can be referenced by later verification gates.

## Risks

- Security: cleanup must validate identifiers before interpolation and use
  parameterized catalog queries. Database URLs in errors and logs must redact
  credentials.
- Compatibility: developers using a database named `harness` for tests will need
  to create/use `harness_test` or set the explicit disposable-db escape hatch.
- Reliability: `Drop` cannot reliably perform async cleanup after runtime
  shutdown. Tests should prefer explicit async cleanup and use `Drop` only as a
  last-resort guard.
- Maintenance: multiple crates currently have local DB availability helpers.
  The safety helper should be small and shared to reduce future drift without
  broad refactoring.

## Test Plan

- [ ] Unit tests: test database URL detection accepts `harness_test`, suffix
      `_test`, prefix `test_`, and the explicit escape hatch.
- [ ] Unit tests: unsafe URL errors redact credentials and query strings.
- [ ] Unit tests: cleanup planning includes only known test-schema prefixes and
      excludes unrelated names.
- [ ] Integration tests: `db_tests_enabled` skips unsafe configured URLs before
      opening a pool.
- [ ] Integration tests: `test_database_url` rejects unsafe configured URLs.
- [ ] Integration tests: `TestSchemaGuard` drops a schema it created in a safe
      database.
- [ ] Verification: `cargo test -p harness-core test_database`.
- [ ] Verification: `cargo test -p harness-server test_database`.
- [ ] Verification: `cargo test -p harness-observe test_schema`.
- [ ] Verification: `cargo check --workspace --all-targets`.

## Rollback Plan

Revert the test safety helper, guard wiring, cleanup command additions, and
documentation updates. No production migrations or persisted data changes are
required. Existing leaked schemas remain untouched unless the explicit cleanup
command has already been applied by an operator.
