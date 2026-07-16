# Product Spec

## Linked Issue

GH-1646

Complexity: medium-high. This is a behavior-preserving internal architecture
change on the legacy task persistence and lifecycle boundary.

## User Problem

Maintainers must review and modify task-store behavior concentrated in the
1,862-line `crates/harness-server/src/task_runner/store.rs`. The file combines
store startup and recovery, cache/database reads, duplicate detection, paging,
runtime-host lease ownership, dashboard projections, task streams, rate
limiting, artifacts, checkpoints, persistence, recovered-PR validation, and
terminal transitions. It exceeds the repository's 800-line hard ceiling and
makes correctness-sensitive changes difficult to isolate.

Although task submission is migrating to the workflow runtime, this legacy
store remains an active compatibility boundary. The optimization must improve
maintainability without changing behavior observed by callers, operators,
runtime hosts, persisted tasks, or existing databases.

## Goals

1. Make every task-store implementation owner cohesive and no larger than the
   repository file-size ceiling.
2. Preserve the complete `TaskStore` API and all existing Rust import paths.
3. Preserve database/cache precedence, lock scope, lease fencing, recovery,
   event-stream, callback, rollback, and terminalization semantics.
4. Give each method and helper one clear implementation owner.
5. Prove the extraction with inventories, isolated PostgreSQL tests, and fresh
   local and exact-head PR evidence.

## Non-Goals

- Removing, redesigning, or migrating callers away from the legacy task layer.
- Changing database schemas, migrations, SQL, persisted task formats, cache
  policy, scheduler state, or recovery policy.
- Introducing a store trait, compatibility alias, new public abstraction, or
  dependency.
- Changing warnings, errors, fallbacks, callbacks, or event ordering.
- Combining this change with another oversized-file cleanup.
- Treating compilation without database-backed tests as sufficient evidence.

## User-Visible Behavior

There is no intended user-visible functional change. Task lookup, duplicate
detection, runtime-host claims, progress streaming, recovery, checkpointing,
abort handling, metrics, and terminal transitions must return the same values
and failures as before. The observable improvement is for maintainers: each
behavior is located in a focused module that can be reviewed independently.

## Testable Invariants

1. B-001: `TaskStore`, `TaskSummaryFilter`, `TaskSummaryPageCursor`,
   `TerminalTransition`, `update_status`, `mark_terminal_once`,
   `mutate_and_persist`, and every existing public or crate-visible method
   remain available at the same Rust path with the same signature.
2. B-002: Every SQL statement, bind order, cache lookup/update, persistence
   lock, runtime-host lease predicate, ordering clause, and transaction
   boundary moved by the change retains its pre-extraction semantics.
3. B-003: Store construction, schema selection, database context, event replay,
   cache hydration, and startup recovery retain their existing sequence and
   failure behavior.
4. B-004: Missing, malformed, stale, and database-error inputs retain their
   existing typed result or propagated error; extraction adds no silent
   fallback, swallowed exception, default data, or panic path.
5. B-005: Concurrent claims, releases, mutations, terminal transitions, and
   rollback checks retain their existing locking and single-winner behavior.
6. B-006: Callback invocation, event logging, task-stream publication/closure,
   abort-handle cleanup, and durable persistence retain their existing order
   and exactly-once guards.
7. B-007: Every pre-extraction type, inherent method, free transition function,
   and private helper exists exactly once in the post-extraction inventory,
   and every existing caller continues through the same root facade.
8. B-008: `crates/harness-server/src/task_runner/store.rs` and every new or
   modified non-exempt production Rust file are at or below 800 lines after
   formatting.
9. B-009: The extraction changes no manifest, lockfile, migration, task model,
   public wire format, persisted format, or externally visible error text.
10. B-010: The implementation cannot be reported complete while isolated
    PostgreSQL tests, formatting, workspace Clippy, full workspace tests,
    exact-head CI, review-thread audit, or SpecRail gates are absent or failing.

## Acceptance Criteria

- [ ] All B-001 through B-010 invariants have fresh verification evidence.
- [ ] The root facade and every touched production module satisfy the 800-line
      ceiling after formatting.
- [ ] The before/after symbol inventory contains no missing or duplicate type,
      method, free function, or helper.
- [ ] Database-backed `harness-server` tests pass against an isolated
      disposable PostgreSQL database.
- [ ] Full workspace tests and warning-sensitive checks pass without weakening
      tests or suppressing diagnostics.
- [ ] Review confirms a mechanical extraction with no behavior or data change.

## Boundary Checklist

| Category | Coverage |
| --- | --- |
| Empty / missing input | covered: B-004 preserves existing absent, malformed, stale, and database-error behavior |
| Error and failure paths | covered: B-003/B-004 preserve startup and runtime failures; B-010 fails closed on missing evidence |
| Authorization / permission | N/A: no authorization policy changes; B-001/B-009 preserve public contracts |
| Concurrency / race / ordering | covered: B-002 preserves locks and predicates; B-005/B-006 preserve single-winner and callback/event order |
| Retry / repetition / idempotency | covered: B-005/B-006 preserve terminal guards, rollback checks, and exactly-once effects |
| Illegal state transitions | covered: B-002/B-005 preserve status predicates and transition locking |
| Compatibility / migration | covered: B-001/B-003/B-009 preserve APIs, startup, schemas, migrations, and formats |
| Degradation / fallback | covered: B-004 forbids new silent fallback; B-010 forbids a partial success claim |
| Evidence and audit integrity | covered: B-007 requires a complete inventory and B-010 requires fresh exact-head evidence |
| Cancellation / interruption / partial completion | covered: B-006 preserves abort and stream cleanup; incomplete extraction cannot pass B-007/B-010 |

## Edge Cases

- A moved method needs a helper owned by another store sibling: expose it only
  as `pub(super)` or `pub(crate)` when required, and do not duplicate it.
- A method spans a database write, cache mutation, stream event, and callback:
  move the complete method so the existing operation order is unchanged.
- A target file approaches 800 lines after formatting: split by cohesive
  responsibility rather than using suppression or relying on pre-format size.
- PostgreSQL connection pressure causes an infrastructure failure: use the
  repository pool limit and serial workspace execution, then rerun from a
  clean isolated database; do not hide a real assertion failure.
- `origin/main` advances during the cycle: start the implementation worktree
  from the then-current remote head and rerun all verification.

## Rollout Notes

No runtime migration or feature flag is required. Rollout is a source-level
refactor. Rollback is a revert of the implementation PR; because schemas,
dependencies, task models, and persisted formats cannot change, rollback does
not require data repair.
