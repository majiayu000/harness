# Tech Spec

## Linked Issue

GH-1635

## Product Spec

`specs/GH1635/product.md`

## Codebase Context

Verified at `origin/main` commit `8729cb4f`.

| Area | Anchor | Current behavior | Relevance |
| --- | --- | --- | --- |
| Pre-push hook | `.githooks/pre-push:1-29` | Runs full clippy, removes DB/pool variables from a workspace test that still includes `harness-workflow`, and conditionally runs only `harness-server` with DB access. | The crate matrix became stale after GH1602. |
| Mandatory workflow DB guard | `crates/harness-workflow/src/runtime/tests/remote_host_lease.rs:10-18` | Six lease tests return an error unless `HARNESS_DATABASE_URL` names an isolated disposable database. | The hook must route these tests; test code must remain unchanged. |
| Global config discovery | `crates/harness-core/src/db_pg.rs:187-216` | Database resolution reads the discovered Harness config and only uses executable-path fallback when `XDG_CONFIG_HOME` is absent. | DB-less commands need an invocation-local empty `XDG_CONFIG_HOME`, not only an unset URL variable. |
| Agent instructions | `AGENTS.md:29-30`, `CLAUDE.md:21-22` | State that pre-push runs non-server tests and only skips `harness-server` without DB configuration. | High-context instructions must be explicitly corrected, not silently rewritten. |
| Contributor instructions | `CONTRIBUTING.md:31-35` | Describes full clippy, non-server tests, and conditional server tests. | Must document workflow DB suite behavior and configuration isolation. |
| Historical hook change | GH-1464 / PR-1553 | Split fast pre-commit from full pre-push and intentionally scoped DB configuration away from non-server tests. | Preserve the performance and environment-isolation intent. |
| Lease-test change | GH-1602 / PR-1622 | Added deterministic fenced-lease PostgreSQL tests after the hook matrix was designed. | Explains the cross-change contract mismatch and why test weakening is forbidden. |

## Current System

The hook has a one-dimensional test classification: `harness-server` may need
PostgreSQL and every other crate does not. It saves no explicit mode state; it
simply unsets database variables around a workspace command. That command now
contains both database-independent workflow tests and mandatory PostgreSQL
workflow tests.

Database URL resolution can also consult a discovered user configuration file.
Consequently `env -u HARNESS_DATABASE_URL` does not prove DB-less execution.

## Proposed Design

### 1. Capture the mode without exposing the URL

At hook startup, determine only whether `HARNESS_DATABASE_URL` is non-empty.
Do not echo or serialize its value. Leave the original environment untouched
for the later DB-configured commands; use `env -u` only on individual DB-less
commands.

Treat an unset or empty URL as DB-less. Optional pool variables remain absent
when absent and flow naturally to DB-configured commands when present.

### 2. Create invocation-local configuration isolation

Create one temporary directory with `mktemp -d` and install an exit/signal trap
that removes it. Pass that path as `XDG_CONFIG_HOME` to every DB-less cargo
command while also unsetting:

- `HARNESS_DATABASE_URL`;
- `HARNESS_DATABASE_POOL_MAX_CONNECTIONS`;
- `HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS`.

This prevents `find_config_file()` from discovering the user Harness config.
The path is never tracked and contains no database secret.

### 3. Use an explicit two-mode test matrix

Always run clippy first:

```text
cargo clippy --workspace --all-targets -- -D warnings
```

Then run environment-sensitive non-database crates in an isolated DB-less
environment:

```text
cargo test --workspace --exclude harness-server --exclude harness-workflow --lib
```

DB-less mode additionally runs `harness-workflow --lib` with a libtest skip
filter scoped to the exact mandatory PostgreSQL module
`runtime::tests::remote_host_lease`. The adjacent pure
`runtime::store::runtime_job_leases` test remains eligible. The hook prints a
stable message that the six explicit PostgreSQL tests plus server DB tests are
deferred.

DB-configured mode instead runs the full suites with the original environment:

```text
cargo test -p harness-workflow --lib
cargo test -p harness-server --lib
```

This avoids running pure workflow tests twice while preserving complete local
coverage when a database exists. No test code or assertion changes.

### 4. Add deterministic hook contract tests

Add a dependency-free executable shell test, for example
`scripts/test-pre-push-hook.sh`, that:

1. Creates a temporary fake `cargo` at the front of `PATH`.
2. Records arguments and boolean/redacted environment facts, never the URL.
3. Invokes `.githooks/pre-push` in DB-less and DB-configured modes.
4. Asserts exact command order and crate exclusions/filters.
5. Asserts DB variables and global config are absent from DB-less calls.
6. Asserts DB presence and pool-variable presence on full workflow/server calls.
7. Makes each command class fail in turn and proves nonzero propagation.
8. Captures output and proves a sentinel secret URL never appears.
9. Proves the hook isolation directory is removed on success and failure.

Use ordinary POSIX/macOS shell tools already required by the repository; add no
test framework dependency. The test must not invoke real cargo builds.

### 5. Correct high-context and contributor instructions

Update only the pre-push bullets/paragraphs in `AGENTS.md`, `CLAUDE.md`, and
`CONTRIBUTING.md`. State:

- clippy always runs;
- DB-less mode runs database-independent workspace/workflow lib tests under an
  isolated config root and defers explicit PostgreSQL suites;
- DB-configured mode runs full workflow and server lib tests;
- an isolated disposable database is required for DB-configured mode.

No other agent policy or workflow rule changes.

## Data Flow

```text
git push
  -> pre-push clippy
  -> isolated DB-less non-workflow workspace lib tests
  -> if isolated URL absent:
       isolated workflow lib tests minus exact PostgreSQL module
       + explicit deferred-coverage message
     else:
       full workflow lib tests with original DB environment
       + full server lib tests with original DB environment
  -> success only when every selected command succeeds
  -> trap removes invocation-local config root
```

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 clippy always first and fatal | `.githooks/pre-push` | fake-cargo order/failure cases in `scripts/test-pre-push-hook.sh`; real `cargo clippy --workspace --all-targets -- -D warnings` |
| B-002 DB-less non-server/non-workflow suite | hook DB-less command helper | fake-cargo exact argv and unset-variable assertions; real hook DB-less run |
| B-003 DB-less workflow suite excludes only mandatory DB module | hook DB-less branch | fake-cargo skip-filter assertion; real `XDG_CONFIG_HOME=<empty> env -u HARNESS_DATABASE_URL cargo test -p harness-workflow --lib -- --skip runtime::tests::remote_host_lease` |
| B-004 global config cannot leak | temporary `XDG_CONFIG_HOME` wrapper | fake-cargo XDG path assertion plus a real run while user global config exists |
| B-005 full workflow/server suites preserve DB environment | hook DB-configured branch | fake-cargo presence/redaction assertions; real isolated-DB hook run |
| B-006 database URL is never disclosed | hook output and test logger | sentinel secret URL absent from captured output and all persistent test files |
| B-007 selected command failure is fatal | `set -euo pipefail` plus command order | fake clippy/non-DB/workflow/server failure cases each return nonzero and omit success marker |
| B-008 temporary state cleans on every exit | hook cleanup trap | shell tests inspect recorded isolation path after success, failure, and signal fixture |
| B-009 git environment remains sanitized | hook startup unset block | fake cargo asserts `GIT_INDEX_FILE`, `GIT_DIR`, and `GIT_WORK_TREE` absent |
| B-010 GH1602 tests unchanged | implementation diff boundary | `git diff origin/main -- crates/harness-workflow/src/runtime/tests/remote_host_lease.rs` is empty |
| B-011 deterministic matrix test | `scripts/test-pre-push-hook.sh` | `bash scripts/test-pre-push-hook.sh` |
| B-012 instructions match hook | three scoped documentation edits | `rg -n 'pre-push|HARNESS_DATABASE_URL' AGENTS.md CLAUDE.md CONTRIBUTING.md` plus review |

## Risks

- Security: a URL may contain credentials. Never echo it or include it in fake
  command logs; tests use presence booleans and a sentinel leak check.
- Logic: a broad `--skip` could hide pure tests. Use the fully qualified
  `runtime::tests::remote_host_lease` module path and assert that the adjacent
  pure lease-store test runs in the real DB-less verification.
- Compatibility: `XDG_CONFIG_HOME` affects config discovery. Scope it to
  DB-less cargo subprocesses only; do not export it for the hook process.
- Reliability: traps under `set -e` can be missed if installed too late.
  Install cleanup immediately after `mktemp` and test failure/signal paths.
- Maintenance: command stubs can validate the wrong hook path. Resolve the
  repository root from the script location and assert the target is executable.
- High-context files: AGENTS/CLAUDE edits can change agent behavior. Limit the
  diff to the existing pre-push bullets and review it explicitly.

## Alternatives Considered

1. Change GH1602 tests to skip when the URL is absent. Rejected: this weakens
   intentional mandatory database evidence and violates test integrity.
2. Require PostgreSQL for every push. Rejected: repository policy explicitly
   supports a DB-less local mode with full DB coverage deferred to CI.
3. Keep one workspace test and pass the database URL to every crate. Rejected:
   environment-sensitive tests would inherit DB configuration, recreating the
   leak PR #1553 intentionally removed.
4. Exclude all `harness-workflow` tests without a DB. Rejected: hundreds of
   database-independent workflow tests would disappear from the hook.
5. Only unset `HARNESS_DATABASE_URL`. Rejected: global config discovery is a
   demonstrated second path to the database.

## Test Plan

- [ ] `bash -n .githooks/pre-push scripts/test-pre-push-hook.sh`.
- [ ] `bash scripts/test-pre-push-hook.sh`.
- [ ] Real DB-less `.githooks/pre-push` on the implementation head.
- [ ] Real DB-configured `.githooks/pre-push` with
      `HARNESS_DATABASE_URL=<isolated-url>`.
- [ ] `cargo fmt --all -- --check`.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `python3 checks/check_workflow.py --repo .`.
- [ ] `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1635`.
- [ ] Exact no-test-change and high-context-diff reviews from the mapping.

## Rollback Plan

Revert the implementation PR. No production data or migration rollback is
needed. Because the previous hook is deterministically incompatible with
GH1602, do not leave the revert as the final state; restore a different
verified two-mode matrix before the next protected push.
