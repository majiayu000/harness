# Product Spec

## Linked Issue

GH-1633

Complexity: medium. This is a behavior-preserving internal architecture change
on the workflow runtime persistence boundary.

## User Problem

Maintainers must review and modify workflow-runtime persistence code that is
concentrated in a 2,213-line `store.rs` file. The file combines unrelated
responsibilities including prompt payloads, workflow events, decisions,
command dispatch, runtime jobs, completion, and projection queries. This is
above the repository's 800-line hard ceiling and makes changes to lease,
transaction, or event-ordering behavior harder to isolate and review.

The optimization must improve the maintainability boundary without changing
the behavior observed by Harness callers, operators, persisted workflows, or
existing databases.

## Goals

1. Make each workflow-runtime store source file cohesive and small enough to
   satisfy the repository file-size constraint.
2. Preserve the complete `WorkflowRuntimeStore` API and all existing call-site
   behavior.
3. Preserve SQL, transaction, locking, ordering, lease, error, and migration
   semantics.
4. Make later persistence work reviewable within one focused responsibility.
5. Prove the extraction with an explicit method inventory and fresh isolated
   PostgreSQL verification.

## Non-Goals

- Changing database schemas, migrations, indexes, or persisted JSON shapes.
- Optimizing or rewriting SQL statements.
- Introducing store traits, new public abstractions, or dependencies.
- Changing workflow, command, runtime-job, or lease behavior.
- Combining this change with other oversized-file or legacy-layer cleanup.
- Treating a compilation-only result as sufficient persistence verification.

## User-Visible Behavior

There is no intended user-visible functional change. Existing workflow
submissions, command dispatch, runtime-job claims, completion, recovery, and
operator projections must produce the same results and failures as before.
The observable improvement is for maintainers: persistence changes can be
located and reviewed in a cohesive module whose size remains within policy.

## Testable Invariants

1. B-001: Every public `WorkflowRuntimeStore` type, re-export, constructor, and
   method available before the extraction remains available at the same Rust
   path with the same signature after the extraction.
2. B-002: Every SQL statement, bind order, transaction boundary, row lock,
   advisory lock, ordering clause, and status predicate moved by the change
   retains its pre-extraction semantics.
3. B-003: Store construction and migration selection continue to use the same
   `PgStoreContext`, schema, migration set, and configured database URL rules.
4. B-004: Empty, missing, malformed, and database-error inputs retain their
   existing typed result or propagated error behavior; extraction must not add
   silent fallback, swallowing, default data, or panic paths.
5. B-005: Concurrent command claim, runtime-job claim, lease renewal,
   completion, cancellation, and reclaim operations retain their existing
   locking and single-winner guarantees.
6. B-006: Repeated idempotent operations retain their existing conflict and
   deduplication behavior; moving a method must not create a second execution
   path or duplicate durable effect.
7. B-007: The post-extraction inventory contains every pre-extraction public
   store method and transaction helper exactly once, and every existing call
   site still resolves through the same store facade.
8. B-008: `crates/harness-workflow/src/runtime/store.rs` and every new or
   modified non-exempt production Rust file are at or below 800 lines.
9. B-009: The extraction changes no dependency manifest, lockfile entry,
   migration, public wire format, or persistence format.
10. B-010: The change cannot be reported complete when isolated PostgreSQL
    verification, package tests, formatting, or warning-sensitive workspace
    checks are missing or failing; missing evidence is an explicit failed gate.

## Acceptance Criteria

- [ ] All B-001 through B-010 invariants have fresh verification evidence.
- [ ] The root store facade and all touched production modules satisfy the
      800-line ceiling.
- [ ] The before/after method and helper inventory has no missing or duplicate
      entry.
- [ ] Database-backed workflow tests pass against an isolated disposable
      PostgreSQL database.
- [ ] Package and workspace readiness checks pass without weakening tests or
      suppressing warnings.
- [ ] Review confirms that the change is mechanical extraction only.

## Boundary Checklist

| Category | Coverage |
| --- | --- |
| Empty / missing input | covered: B-004 preserves all existing empty, missing, malformed, and database-error behavior |
| Error and failure paths | covered: B-004 forbids swallowed errors, fallback data, and new panic paths; B-010 fails closed on missing verification |
| Authorization / permission | N/A: no authorization policy or route is changed; B-001 and B-009 prohibit public contract changes |
| Concurrency / race / ordering | covered: B-002 preserves locks and ordering; B-005 preserves single-winner concurrency behavior |
| Retry / repetition / idempotency | covered: B-006 preserves deduplication and prevents duplicate execution paths |
| Illegal state transitions | covered: B-002 preserves status predicates and transactional state boundaries; no transition vocabulary changes |
| Compatibility / migration | covered: B-001, B-003, and B-009 preserve APIs, startup, schemas, migrations, and formats |
| Degradation / fallback | covered: B-004 forbids new fallback or silent success; B-010 treats unavailable evidence as failure |
| Evidence and audit integrity | covered: B-007 requires a complete inventory and B-010 requires fresh verification evidence |
| Cancellation / interruption / partial completion | covered: B-005 preserves cancellation/lease races; an interrupted source edit cannot pass B-007/B-010 |

## Edge Cases

- A method uses a private helper that moves to a sibling module: visibility may
  be widened only to `pub(super)` or `pub(crate)` as narrowly required; the
  helper must still have one owner.
- A SQL path shares transaction-local helpers with completion and recovery:
  the helper moves once and all callers import that owner rather than cloning
  the SQL.
- A target module approaches 800 lines after formatting: split by cohesive
  responsibility instead of claiming the pre-format count.
- An isolated database is unavailable: compilation may still be reported, but
  the implementation gate remains failed under B-010.
- `origin/main` advances during the spec or implementation cycle: rebase or
  recreate the implementation branch from the current remote base and rerun
  the full verification set.

## Rollout Notes

No runtime migration or feature flag is required. The implementation is a
normal source-level refactor. Rollback is a revert of the implementation PR;
because no schema, dependency, or wire-format change is allowed, rollback does
not require data repair.
