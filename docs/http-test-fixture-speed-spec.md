# HTTP Route Test Fixture Speed Spec

## Goal

Reduce `harness-server` HTTP route test latency by letting read-only route tests build a smaller `AppState`.

The target is Issue #1190: replace full `AppState` setup in route tests that only exercise HTTP response shaping, auth middleware, task listing, or health metadata. Tests that verify runtime stores, webhooks, event persistence, thread persistence, or execution behavior stay on the full fixture.

## Problem

`crates/harness-server/src/http/tests.rs` mixed two concerns:

- End-to-end HTTP workflow tests that need workflow runtime stores, event persistence, agents, and execution services.
- Read-only route tests that only need a router, config, task store, and shallow `AppState` fields.

The second group paid the full setup cost for `EventStore`, `ThreadDb`, project registry, and real execution service wiring even when the route never touched those systems.

## Design

Add a `#[cfg(test)]` read-only HTTP route fixture in `crates/harness-server/src/http/test_fixtures.rs`.

The fixture builds:

- Real `HarnessServer` from the supplied config.
- Real `TaskStore`, because several route handlers still read `state.core.tasks` directly.
- In-memory `ProjectService`.
- Fail-closed `ExecutionService` that returns an internal enqueue error if a migrated test accidentally reaches execution.
- No-op `EventStore`, no `ThreadDb`, no project registry, no workflow runtime stores.
- Normal rate limiters, notification channels, queues, skill/rule stores, and GC agent so router shape remains realistic.

## Migration Rules

Migrate a test only when all of these are true:

- The route does not enqueue tasks.
- The assertion does not depend on persisted events.
- The assertion does not depend on thread persistence.
- The assertion does not require workflow runtime, issue workflow, project workflow, or project registry persistence.
- A fail-closed execution service would catch accidental scope drift.

Keep the full fixture for runtime worker tests, webhook dispatch tests, recovery tests, SSE replay tests, and workflow store tests.

## Mechanical Split

The legacy HTTP test file exceeded the repository file-size limit. Split it into focused modules under `crates/harness-server/src/http/tests/` before migrating individual tests. The split is intended to preserve behavior while making later route fixture migrations reviewable.

## Verification

Required local verification for this PR:

1. `cargo fmt --all`
2. `cargo check -p harness-server`
3. `cargo test -p harness-server --lib http::tests:: -- --test-threads=1 --format terse`
4. `cargo test -p harness-server --lib http:: -- --test-threads=1 --format terse`
5. `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets`
6. `cargo test -p harness-server --lib -- --test-threads=1 --format terse`

## Acceptance Criteria

- The split HTTP test modules compile.
- Migrated route tests pass with the read-only fixture.
- Full `harness-server` lib tests pass single-threaded.
- CI-equivalent warnings check passes.
- The PR documents the fixture boundary so future route tests can choose the cheaper setup safely.
