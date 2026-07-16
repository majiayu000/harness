# Product Spec

## Linked Issue

GH-1635

Complexity: medium. This changes the local delivery gate and its documented
database/no-database contract without changing production runtime behavior.

## User Problem

Contributors with an up-to-date checkout cannot rely on the pre-push hook. The
hook removes `HARNESS_DATABASE_URL` and runs all non-server workspace lib tests,
but `harness-workflow` now contains six PostgreSQL lease tests that intentionally
fail when that variable is missing. The DB-less hook mode therefore fails on a
clean branch.

Unsetting the variable also does not isolate the process from the user global
Harness config. A configured but unavailable endpoint can be rediscovered and
turn an immediate deterministic failure into many 30-second pool timeouts.
Contributors are pushed toward `--no-verify`, losing the protection the hook is
meant to provide.

## Goals

1. Give the pre-push hook explicit DB-less and DB-configured test matrices.
2. Run all database-independent workspace and workflow lib tests in DB-less
   mode without reading user global Harness database configuration.
3. Run the complete `harness-workflow` and `harness-server` lib suites when an
   isolated `HARNESS_DATABASE_URL` is configured.
4. Preserve mandatory full workspace clippy and fail-fast behavior.
5. Prove command selection, environment isolation, secret handling, cleanup,
   and failure propagation with deterministic hook tests.
6. Keep contributor and agent instructions aligned with the executable hook.

## Non-Goals

- Weakening, deleting, or silently skipping GH1602 PostgreSQL tests when a
  database is configured.
- Changing a production database URL resolver or global configuration rules.
- Requiring PostgreSQL for every local push.
- Changing CI path filters, test assertions, or product behavior.
- Adding a new shell-test dependency.
- Combining runtime-store modularization or other developer-tooling work.

## User-Visible Behavior

When no isolated database URL is configured, pre-push runs clippy, all
database-independent non-server lib tests, and the database-independent
`harness-workflow` lib tests. It reports exactly which PostgreSQL suites are
deferred to CI or a configured local database and succeeds when those executed
checks pass.

When an isolated database URL is configured, pre-push runs clippy, the
environment-isolated non-database workspace tests, then the complete
`harness-workflow` and `harness-server` lib suites with the original database
and pool settings. The hook never prints the URL.

## Testable Invariants

1. B-001: Both modes run `cargo clippy --workspace --all-targets -- -D warnings`
   before any test command, and a clippy failure stops the hook.
2. B-002: DB-less mode runs non-server, non-workflow workspace lib tests with
   `HARNESS_DATABASE_URL` and related pool variables absent.
3. B-003: DB-less mode runs the `harness-workflow` lib suite while excluding
   only tests whose declared contract requires an isolated PostgreSQL URL; all
   other workflow tests remain eligible and any executed failure is fatal.
4. B-004: Every DB-less test command uses an isolated empty configuration root
   so user global Harness config cannot provide a database URL or pool setting.
5. B-005: DB-configured mode runs the complete `harness-workflow` and
   `harness-server` lib suites with the original non-empty
   `HARNESS_DATABASE_URL` and optional pool variables preserved exactly.
6. B-006: The hook never prints, interpolates into diagnostics, or stores the
   database URL in a tracked or persistent temporary file.
7. B-007: A failure from clippy or any selected test command returns a nonzero
   hook status and prevents later success output.
8. B-008: Temporary configuration isolation is unique per invocation and is
   removed on success, command failure, or interruption.
9. B-009: Git hook environment variables that can corrupt child test
   repositories remain unset for every spawned cargo command.
10. B-010: GH1602 test code and assertions remain unchanged; the hook selects
    the appropriate suite instead of modifying tests to manufacture green.
11. B-011: Deterministic hook tests prove the command/environment matrix for
    both modes, pool-variable propagation, secret non-disclosure, failure
    propagation, and cleanup without running the real workspace suite.
12. B-012: `AGENTS.md`, `CLAUDE.md`, and `CONTRIBUTING.md` describe the same
    DB-less and DB-configured behavior as the executable hook and retain the
    human/CI coverage boundary.

## Acceptance Criteria

- [ ] DB-less pre-push passes on clean `origin/main` and runs every selected
      database-independent test.
- [ ] DB-less mode cannot rediscover the user global Harness config.
- [ ] DB-configured pre-push passes against an isolated disposable database
      and runs complete workflow/server lib suites.
- [ ] B-001 through B-012 have fresh automated or exact command evidence.
- [ ] No production Rust code, GH1602 test assertion, dependency, or CI
      workflow is changed.
- [ ] All three instruction documents match the implemented matrix.

## Boundary Checklist

| Category | Coverage |
| --- | --- |
| Empty / missing input | covered: B-002/B-003 define absent URL behavior; B-005 requires a non-empty URL before DB-configured mode |
| Error and failure paths | covered: B-007 fails fast for every selected command; B-011 exercises failures |
| Authorization / permission | N/A: local hook selection has no authorization policy; database safety remains enforced by existing tests |
| Concurrency / race / ordering | covered: B-001 orders clippy before tests; B-008 requires invocation-unique temporary state and signal cleanup |
| Retry / repetition / idempotency | covered: B-008 makes repeated/concurrent hook invocations use independent temporary roots |
| Illegal state transitions | N/A: the hook has no persisted state machine; B-007 prevents failed commands from reaching success output |
| Compatibility / migration | covered: B-010 preserves tests; B-012 preserves and corrects the existing documented local/CI contract |
| Degradation / fallback | covered: B-003 explicitly scopes deferred tests and forbids treating executed failures as skip/success |
| Evidence and audit integrity | covered: B-011 proves the matrix; B-012 aligns instructions; missing mode evidence blocks completion |
| Cancellation / interruption / partial completion | covered: B-008 cleans temporary state on normal exit, failure, and interruption |

## Edge Cases

- `HARNESS_DATABASE_URL` is set to an empty string: treat it as DB-less and do
  not pass the empty variable into tests.
- One or both pool variables are absent: preserve absence rather than inventing
  a default in DB-configured commands.
- A global Harness config exists and points to a live database: DB-less mode
  still must not connect to it.
- A fake cargo command exits nonzero during deterministic hook tests: the hook
  must return that failure and remove its temporary configuration root.
- Two hooks run concurrently: their isolation roots and command logs do not
  collide.
- The database URL contains credentials or shell-sensitive characters: pass it
  only through the environment, never through output or command construction.

## Rollout Notes

The hook and its tests land together. No runtime migration or feature flag is
needed. Rollback reverts the implementation PR, but the old hook remains known
broken after GH1602; rollback should therefore occur only in favor of another
verified test matrix.
