# Task Plan

## Linked Issue

GH-1456

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1456-T001` Owner: `server-test-helper` | Dependencies: GH-1444 merged | Done when: `DB_STATE_LOCK` no longer exists as a serializing mutex, `acquire_db_state_guard()` is removed or converted to a non-serializing compatibility shim, and a focused test proves overlapping acquisitions do not block each other | Verify: `cargo test -p harness-server db_state_guard -- --nocapture`
- [ ] `SP1456-T002` Owner: `server-state-isolation` | Dependencies: `SP1456-T001` | Done when: `make_state_inner` and representative DB-backed server state tests rely on unique temp directories or `TestSchemaGuard`, not database-wide serialization | Verify: `cargo test -p harness-server shared_schema -- --nocapture && cargo test -p harness-server backfill -- --nocapture`
- [ ] `SP1456-T003` Owner: `integration-lock-cleanup` | Dependencies: `SP1456-T001` | Done when: integration-local DB-only locks are removed or made non-serializing, while locks for `HOME` or environment mutation remain explicit and documented | Verify: `cargo test -p harness-server --test gc_adopt_pipeline -- --nocapture && cargo test -p harness-server --test turn_start_lifecycle -- --nocapture`
- [ ] `SP1456-T004` Owner: `parallel-runner` | Dependencies: `SP1456-T001`, `SP1456-T002`, `SP1456-T003` | Done when: `scripts/test-server-db.sh` no longer forces `RUST_TEST_THREADS=1`, the DB-capable nextest path is documented or configured, and intentionally serial cases are called out explicitly | Verify: `scripts/test-server-db.sh --help` or equivalent usage output plus `cargo nextest run --workspace` when nextest is installed
- [ ] `SP1456-T005` Owner: `verification` | Dependencies: all implementation tasks | Done when: focused DB suites, fallback cargo tests, formatting, clippy, and SpecRail checks pass, and before/after timing evidence for the main server DB profile is recorded in the PR body or artifact | Verify: `cargo fmt --all -- --check && cargo check --workspace --all-targets && cargo clippy --workspace --all-targets -- -D warnings && python3 checks/check_workflow.py --repo . --spec-dir specs/GH1456 && python3 checks/check_workflow.py --repo .`

## Parallelization

`SP1456-T001` should land first because the helper contract determines whether
call sites can be cleaned mechanically or left as compatibility no-ops.
`SP1456-T002` and `SP1456-T003` can proceed in parallel after that if file
ownership is disjoint. `SP1456-T004` depends on the tests being safe to run in
parallel. `SP1456-T005` is the final gate.

## Verification

- `cargo test -p harness-server db_state_guard -- --nocapture`
- `cargo test -p harness-server shared_schema -- --nocapture`
- `cargo test -p harness-server backfill -- --nocapture`
- `cargo test -p harness-server --test gc_adopt_pipeline -- --nocapture`
- `cargo test -p harness-server --test turn_start_lifecycle -- --nocapture`
- `cargo test -p harness-core db_test_safety -- --nocapture`
- `cargo test -p harness-cli schema_cleanup -- --nocapture`
- `cargo test -p harness-server --lib`
- `cargo nextest run --workspace` when installed, or the documented DB-capable nextest subset
- `cargo check --workspace --all-targets`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1456`
- `python3 checks/check_workflow.py --repo .`

## Handoff Notes

GH-1444 is a prerequisite and already owns non-test database rejection plus
known temporary schema cleanup. GH-1456 must not undo that safety work. The
implementation should treat `HOME_LOCK` and env locks as real process-global
protection, while treating database-wide locks as compatibility debt to remove
or neutralize. Do not close GH-1456 from a spec-only PR; use a closing keyword
only on the final implementation PR that satisfies all acceptance criteria.
