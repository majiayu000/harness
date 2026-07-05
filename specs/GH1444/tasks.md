# Task Plan

## Linked Issue

GH-1444

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1444-T001` Owner: `test-db-safety` | Done when: a shared helper validates test-designated database URLs, redacts unsafe URL diagnostics, and supports the explicit disposable-database escape hatch | Verify: `cargo test -p harness-core test_database`
- [ ] `SP1444-T002` Owner: `test-helper-wiring` | Done when: core, server, observe, and workflow Postgres-backed test setup paths validate safe database URLs before opening pools or return their existing skip path | Verify: `cargo test -p harness-server test_database && cargo test -p harness-observe test_database`
- [ ] `SP1444-T003` Owner: `schema-cleanup` | Done when: named `*_test_*` schema helpers use an RAII cleanup guard that validates identifiers and drops owned schemas on normal completion with panic-unwind backup behavior | Verify: `cargo test -p harness-observe test_schema`
- [ ] `SP1444-T004` Owner: `cleanup-tooling` | Done when: `harness-pg-schema-cleanup` can dry-run known leaked test schemas and requires explicit apply flags before dropping validated names | Verify: `cargo test -p harness-cli pg_schema_cleanup`
- [ ] `SP1444-T005` Owner: `docs-verification` | Done when: local DB test documentation points to `harness_test`, SpecRail checks pass, and focused plus workspace checks are recorded | Verify: `cargo fmt --all -- --check && cargo check --workspace --all-targets && python3 checks/check_workflow.py --repo . --spec-dir specs/GH1444`

## Parallelization

T001 should land first because every other task depends on the shared safety
contract. T002 and T003 can proceed after the helper API is stable. T004 can be
implemented independently once the allowed leaked-schema prefix list is fixed.
T005 is the final verification and documentation pass.

## Verification

- `cargo test -p harness-core test_database`
- `cargo test -p harness-server test_database`
- `cargo test -p harness-observe test_database`
- `cargo test -p harness-observe test_schema`
- `cargo test -p harness-cli pg_schema_cleanup`
- `cargo check --workspace --all-targets`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1444`
- `python3 checks/check_workflow.py --repo .`

## Handoff Notes

Keep the production database resolution path unchanged. The implementation must
only add safety around test setup and explicit cleanup tooling. Do not weaken DB
tests into no-op assertions; if an unsafe URL is detected, skip only through the
existing optional-DB behavior or fail before opening a pool where the test
requires Postgres.
