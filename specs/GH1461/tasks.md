# Task Plan

## Linked Issue

GH-1461

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1461-T001` Owner: `catalog-census-model` | Dependencies: none | Done when: a serializable catalog census payload and threshold evaluation helper exist with snake_case JSON fields and sanitized error states | Verify: threshold unit tests for absolute, relative, no-breach, and unavailable states
- [ ] `SP1461-T002` Owner: `bounded-census-query` | Dependencies: `SP1461-T001` | Done when: the server can collect `pg_namespace`, `pg_class`, and `pg_database_size(current_database())` through catalog-only queries and cache the latest sample behind a bounded refresh interval | Verify: DB-backed or fake-pool tests prove query shape, cache reuse, and failure handling
- [ ] `SP1461-T003` Owner: `health-surface` | Dependencies: `SP1461-T001`, `SP1461-T002` | Done when: `/health` includes `postgres_catalog`, keeps the route available on census errors, and marks/logs threshold breaches loudly | Verify: health route tests with lowered thresholds and unavailable/no-Postgres state
- [ ] `SP1461-T004` Owner: `usage-monitor-api` | Dependencies: `SP1461-T001`, `SP1461-T002` | Done when: `/api/usage-monitor` includes the same catalog census payload without issuing independent unbounded queries | Verify: usage monitor handler tests include available and unavailable catalog payloads
- [ ] `SP1461-T005` Owner: `usage-monitor-ui` | Dependencies: `SP1461-T004` | Done when: usage monitor types and UI render schema count, catalog object count, DB size, breach state, and unavailable state | Verify: focused web tests for `UsageMonitor`
- [ ] `SP1461-T006` Owner: `verification` | Dependencies: all implementation tasks | Done when: focused Rust/web tests, SpecRail checks, formatting, and clippy pass | Verify: focused test commands, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1461`, `python3 checks/check_workflow.py --repo .`, `cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

`SP1461-T001` is the foundation. `SP1461-T003`, `SP1461-T004`, and
`SP1461-T005` touch different surfaces after the model/query helper exists, but
they share the response schema and should not proceed with divergent field
names. Keep writable ownership disjoint if parallel agents are used.

## Verification

- `cargo test -p harness-server health_route_tests::`
- focused usage monitor handler tests
- focused web tests for `UsageMonitor`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1461`
- `python3 checks/check_workflow.py --repo .`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Handoff Notes

Use `Refs #1461` for the spec PR. The implementation PR should use a closing
keyword only after `/health`, `/api/usage-monitor`, UI, threshold logging, and
verification are all complete.
