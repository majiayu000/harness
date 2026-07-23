# Tech Spec

## Linked Issue

GH-1716

## Product Spec

See `specs/GH1716/product.md`.

<!-- specrail-planned-changes
{"issue":1716,"complete":true,"paths":["crates/harness-server/src/event_replay.rs","crates/harness-server/src/event_replay_tests.rs","crates/harness-server/src/task_db.rs","crates/harness-server/src/task_db/queries_recovery.rs","crates/harness-server/src/task_db/queries_recovery_tests.rs","crates/harness-server/src/task_runner/store/startup.rs","crates/harness-server/tests/checkpoint_recovery.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010"]}
-->

## Current System

- `crates/harness-server/src/http/builders/storage.rs:76-95` opens the retained
  legacy task store as critical server storage.
- `crates/harness-server/src/task_runner/store/startup.rs:67-92` runs event
  replay before checkpoint recovery. Replay errors are logged as non-fatal;
  checkpoint recovery errors fail task-store startup.
- `crates/harness-server/src/task_db/queries_recovery.rs:48-58` and `:70-79`
  execute the terminal-status and PR-only replay writes without inspecting
  `rows_affected()`.
- `crates/harness-server/src/task_db/queries_recovery.rs:149-160`,
  `:168-178`, `:196-207`, and `:229-240` execute four checkpoint/transient
  recovery writes without inspecting `rows_affected()`.
- `crates/harness-server/src/task_db/queries_recovery.rs:180-208` increments
  `resumed` or `failed` after the ignored result, while `:242-249` derives
  `transient_failed` from the selected-row count rather than committed writes.
- `crates/harness-server/src/event_replay.rs:725-736` increments `updated`
  after every apparently successful method call. It then compacts terminal
  replay evidence at `:739-752`.
- `crates/harness-server/src/task_db/types.rs:52-60` exposes only applied
  recovery aggregates and requires no schema or serialization change.
- Ordinary task writes already fail on a zero-row optimistic update at
  `crates/harness-server/src/task_db/queries_tasks.rs:128-134`,
  `:315-321`, `:344-350`, and `:373-379`, but those errors are stringly
  `anyhow` values rather than a reusable typed conflict contract.

## Proposed Design

1. Define a crate-private `TaskRecoveryWriteOutcome` at the `task_db` module
   boundary with `Applied`, `Superseded`, and
   `Conflict { action, task_id, expected_version, current_version }`.
   `Conflict` carries structured context until the recovery caller converts it
   into an explicit error.
2. Capture every recovery UPDATE result and accept `Applied` only when
   `rows_affected() == 1`. Treat any value greater than one as an invariant
   violation even though `(store_key, id)` uniqueness should prevent it.
3. On zero affected rows, reload the minimum authoritative task fields needed
   by the current action and classify conservatively:
   - `Superseded` when the row is absent, already contains the exact intended
     result, or has reached an action-specific state that conclusively makes
     the action obsolete without contradicting its evidence.
   - `Conflict` when the row lacks the intended result and remains eligible or
     contains contradictory durable evidence. Failure of the original
     eligibility predicate alone never proves supersession.
4. Keep classification action-specific. Terminal replay checks resumable
   status and requires the complete intended durable result before
   superseding: the exact terminal state plus the same PR URL whenever replay
   supplies one. A nonmatching terminal state, missing required replay PR, or
   different PR conflicts. PR-only replay likewise requires the same PR URL.
   Checkpoint resume checks resumable status plus the exact
   pending/scheduler/PR target, while no-checkpoint and transient recovery
   classify only explicit newer terminal outcomes as action-obsoleting.
5. Do not retry `Conflict` within the same invocation. A blind reload-and-write
   loop could overwrite a live writer and defeat optimistic locking. Return an
   error so a later startup or explicit rerun begins from a fresh snapshot.
6. Increment `RecoveryResult` fields and emit action success logs only for
   `Applied`. Route outcome-to-log wording through one inspectable decision
   function: `Superseded` produces bounded non-success diagnostics and
   `Conflict` produces an error before aggregate success logging. Test this
   decision with a test-local tracing subscriber and captured writer.
7. Return the replay write outcome from `apply_replayed_state`. In
   `replay_and_recover`, increment `updated` only for `Applied`, skip counting
   `Superseded`, and return before `compact_log` on `Conflict`.
8. Preserve a typed replay-conflict error through `replay_and_recover`.
   `TaskStore::from_task_db_with_startup_recovery` must propagate that conflict
   and stop before checkpoint recovery. It may retain the current non-fatal
   policy for explicitly non-conflict replay/compaction errors, but must not
   erase the typed conflict through generic logging.
9. Preserve all existing SQL predicates, target states, checkpoint precedence,
   scheduler-state mutations, and startup ordering. Do not modify workflow
   runtime stores or recovery APIs.

## Data Flow

`startup -> replay/select current task version -> version-guarded UPDATE ->
affected-row check -> Applied | fresh authoritative read -> Superseded |
Conflict`.

For `Applied`, recovery updates the matching aggregate and may emit success
evidence. For `Superseded`, recovery emits non-success diagnostics and moves to
the next task. For `Conflict`, recovery returns a typed error. Event replay
reaches terminal-log compaction only after every replayed task is either
applied or proven superseded; startup propagates the conflict before entering
checkpoint recovery.

No external API call, schema write, workflow runtime event, decision, command,
or artifact is added.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | `task_db.rs`, `task_db/queries_recovery.rs` | `cargo test -p harness-server --lib task_db::queries_recovery_tests::write_outcomes_are_closed_and_exclusive` |
| B-002 | `task_db/queries_recovery.rs`, `event_replay.rs` | `cargo test -p harness-server --lib task_db::queries_recovery_tests::recovery_counts_and_success_logs_require_applied_write` captures tracing output and counters |
| B-003 | action-specific fresh-read classifiers | `cargo test -p harness-server --lib task_db::queries_recovery_tests::lost_cas_is_superseded_only_with_authoritative_evidence` |
| B-004 | conflict outcome and error conversion | `cargo test -p harness-server --lib task_db::queries_recovery_tests::contradictory_or_still_eligible_stale_write_is_conflict` covers a different PR URL and nonmatching terminal result |
| B-005 | `event_replay.rs`, `event_replay_tests.rs`, `task_runner/store/startup.rs` | `cargo test -p harness-server --lib event_replay::tests::replay_conflict_preserves_terminal_log`; `cargo test -p harness-server --lib task_runner::store::startup::tests::replay_conflict_fails_startup_before_checkpoint_recovery` |
| B-006 | no in-call retry and repeat behavior | `cargo test -p harness-server --lib task_db::queries_recovery_tests::repeated_recovery_converges_without_rewrite` |
| B-007 | existing SQL/decode propagation plus new classifier | `cargo test -p harness-server --test task_db_rounds`; existing corrupted scheduler-state tests remain green |
| B-008 | existing recovery policy | `cargo test -p harness-server --test checkpoint_recovery` |
| B-009 | centralized outcome-to-log decision and aggregate branches | `cargo test -p harness-server --lib task_db::queries_recovery_tests::recovery_counts_and_success_logs_require_applied_write` uses a test-local tracing subscriber/captured writer to reject success wording for `Superseded` and `Conflict` |
| B-010 | exact path scope and package checks | `git diff --name-only origin/main...HEAD`; `cargo check -p harness-server --all-targets` |

## PostgreSQL Interleaving Tests

- Use isolated PostgreSQL-backed task stores and two actors.
- Actor A captures an eligible row and version. Actor B commits a version bump
  before actor A invokes the extracted CAS application/classification helper.
  This deterministic stale snapshot avoids timing-based sleeps.
- For `Applied`, leave the selected version current and assert one affected
  row, one aggregate increment, and the intended durable status/scheduler/PR
  fields.
- For `Superseded`, have actor B delete the row, apply the exact equivalent
  target, or move it to an explicitly enumerated action-obsoleting state that
  does not contradict the evidence. Assert no rewrite and no success count.
- For `Conflict`, have actor B bump the version while leaving the row eligible
  without the target result, store a different PR URL, or store a nonmatching
  terminal replay result. Assert structured conflict context and no count.
- Repeat the applied/superseded/conflict matrix for all six UPDATE sites:
  terminal replay, PR-only replay, checkpoint resume with PR writeback,
  checkpoint resume without PR writeback, no-checkpoint failure, and
  transient-retry failure. Each case asserts that its own `PgQueryResult` is
  inspected rather than relying only on shared classifier tests.
- For replay, create terminal JSONL evidence, force the stale eligible row
  conflict, and assert replay returns an error before compaction; rerun after
  applying an equivalent complete terminal result, including the expected PR
  URL when supplied, and assert idempotent supersession.

## Alternatives Considered

- Check only `rows_affected() == 0` and return a generic error: rejected
  because an exact equivalent or explicitly action-obsoleting newer state is a
  safe, diagnosable supersession rather than an unresolved conflict.
- Retry automatically with the fresh version: rejected because the competing
  process may still own the row and recovery would overwrite newer work.
- Count selected rows and add warning logs: rejected because selection is not
  evidence of a committed state change.
- Wrap all legacy task writes in a new repository/service layer: rejected as
  unrelated architecture scope.
- Modify workflow runtime recovery: rejected because it already has separate
  transactional decision and conflict contracts.

## Risks

- Security: no authentication, authorization, secret, or external command
  surface changes.
- Data integrity: an overly broad `Superseded` classifier could hide a lost
  recovery. Each action therefore requires a fresh read and an explicit
  eligibility/equivalence predicate.
- Compatibility: `RecoveryResult` retains its public shape and counts applied
  writes only; no persisted schema or serialized data changes.
- Availability: a genuine conflict can fail critical legacy task-store
  startup. This is intentional fail-closed behavior and must include enough
  context for an operator to identify the competing writer.
- Performance: fresh reads occur only after a zero-row CAS result, so the
  uncontended startup path adds no query.
- Maintenance: every new recovery action must define its eligibility,
  equivalence, and conflict classification explicitly.

## Test Plan

- [ ] Run all commands in the Product-to-Test Mapping.
- [ ] Run the deterministic PostgreSQL applied/superseded/conflict matrix for
      terminal replay, PR-only replay, checkpoint resume with PR writeback,
      checkpoint resume without PR writeback, no-checkpoint failure, and
      transient-retry failure.
- [ ] Capture tracing with a test-local subscriber and assert every success
      counter and success-worded log branch is reached only after one affected
      row; `Superseded` and `Conflict` output must contain no success wording.
- [ ] Assert an unresolved replay conflict leaves terminal event-log contents
      byte-for-byte unchanged.
- [ ] Assert the typed replay conflict exits task-store startup before
      checkpoint recovery, while a separately classified non-conflict
      compaction failure follows the existing non-fatal policy.
- [ ] Run `cargo test -p harness-server --lib event_replay`.
- [ ] Run `cargo test -p harness-server --test checkpoint_recovery`.
- [ ] Run `cargo test -p harness-server --test task_db_rounds`.
- [ ] Run `cargo check -p harness-server --all-targets`.
- [ ] Run `cargo fmt --all -- --check` and
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run PostgreSQL-backed tests with an isolated disposable
      `HARNESS_DATABASE_URL`; otherwise require exact-head CI for those tests.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1716`.
- [ ] Collect exact-head CI, Gemini review, independent reviewer evidence,
      review-thread state, and SpecRail PR-gate evidence before merge readiness.

## Rollback Plan

Squash-revert the implementation PR. No schema rollback, data migration,
configuration change, or workflow runtime repair is required. If rollback is
necessary while concurrent legacy writers exist, stop the competing writer
before restarting rather than accepting false recovery evidence.
