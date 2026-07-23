# Task Plan

## Linked Issue

GH-1716

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [x] `SP1716-T1` Owner: recovery-contract implementation agent | Dependencies: approved GH-1716 product and tech specs | Done when: every recovery CAS resolves to one typed closed outcome after an action-specific authoritative read | Verify: focused recovery classifier tests below | Covers: B-001, B-003, B-004, B-006, B-007
- [x] `SP1716-T2` Owner: event-replay implementation agent | Dependencies: SP1716-T1 | Done when: replay counts only applied writes, preserves terminal evidence on conflict, and propagates typed conflicts through startup | Verify: event replay and startup-focused tests below | Covers: B-002, B-005, B-006, B-007, B-009, B-010
- [x] `SP1716-T3` Owner: startup-recovery implementation agent | Dependencies: SP1716-T1 | Done when: all four checkpoint/transient CAS sites inspect affected rows and aggregate/log only applied outcomes without changing existing recovery policy | Verify: focused recovery and checkpoint tests below | Covers: B-001, B-002, B-003, B-004, B-006, B-007, B-008, B-009, B-010
- [x] `SP1716-T4` Owner: PostgreSQL verification owner | Dependencies: SP1716-T2 and SP1716-T3 | Done when: deterministic applied, superseded, and conflict interleavings cover all six version-guarded write sites and replay conflict leaves terminal JSONL evidence unchanged | Verify: PostgreSQL-backed recovery matrix and replay/checkpoint suites below | Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010
- [x] `SP1716-T5` Owner: coordinator | Dependencies: SP1716-T4 | Done when: formatting, package checks, focused suites, SpecRail checks, and exact diff-scope review are green or any unavailable PostgreSQL gate is explicitly deferred | Verify: repository gates below | Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010

### SP1716-T1 — Define closed recovery-write outcomes

- Owner: recovery-contract implementation agent.
- Dependencies: approved GH-1716 product and tech specs.
- Covers: B-001, B-003, B-004, B-006, B-007.
- Work: define the crate-private `TaskRecoveryWriteOutcome`, inspect every
  affected-row result, reload authoritative task fields after a lost CAS, and
  classify each action conservatively as `Applied`, `Superseded`, or
  `Conflict`. Preserve database, serialization, and scheduler decoding errors
  as errors.
- Done when: exactly one affected row is the only applied case; more than one
  row is an invariant error; zero rows require a fresh action-specific read;
  equivalent, absent, and explicitly obsolete rows supersede; eligible or
  contradictory rows return structured conflict context; no conflict is
  retried within the invocation.
- Verify:
  `cargo test -p harness-server --lib task_db::queries_recovery_tests::write_outcomes_are_closed_and_exclusive`;
  `cargo test -p harness-server --lib task_db::queries_recovery_tests::lost_cas_is_superseded_only_with_authoritative_evidence`;
  `cargo test -p harness-server --lib task_db::queries_recovery_tests::contradictory_or_still_eligible_stale_write_is_conflict`;
  `cargo test -p harness-server --lib task_db::queries_recovery_tests::repeated_recovery_converges_without_rewrite`.

### SP1716-T2 — Fail closed through event replay and startup

- Owner: event-replay implementation agent.
- Dependencies: SP1716-T1.
- Covers: B-002, B-005, B-006, B-007, B-009, B-010.
- Work: return the typed write outcome from replay application, count only
  `Applied`, skip success wording for `Superseded`, abort before compaction on
  `Conflict`, and preserve the typed replay conflict through task-store startup
  before checkpoint recovery. Keep explicitly non-conflict replay and
  compaction failures on their documented non-fatal startup path.
- Done when: an unresolved terminal replay conflict leaves the JSONL log
  byte-for-byte unchanged and fails store startup before checkpoint recovery;
  equivalent replay state is idempotently superseded; non-conflict compaction
  failure retains existing startup behavior.
- Verify:
  `cargo test -p harness-server --lib event_replay`;
  `cargo test -p harness-server --lib event_replay::tests::replay_conflict_preserves_terminal_log`;
  `cargo test -p harness-server --lib task_runner::store::startup::tests::replay_conflict_fails_startup_before_checkpoint_recovery`.

### SP1716-T3 — Make checkpoint recovery aggregates durable

- Owner: startup-recovery implementation agent.
- Dependencies: SP1716-T1.
- Covers: B-001, B-002, B-003, B-004, B-006, B-007, B-008, B-009, B-010.
- Work: apply the outcome contract to checkpoint resume with PR writeback,
  checkpoint resume without PR writeback, no-checkpoint failure, and
  transient-retry failure. Increment `resumed`, `failed`, and
  `transient_failed` and emit success wording only after `Applied`; emit
  bounded non-success diagnostics for `Superseded`; convert `Conflict` to an
  explicit error.
- Done when: all four startup UPDATE results are inspected, aggregate counts
  equal committed writes, and the existing PR/plan/triage resume,
  no-checkpoint failure, and transient-retry failure policies are unchanged in
  uncontended recovery.
- Verify:
  `cargo test -p harness-server --lib task_db::queries_recovery_tests::recovery_counts_and_success_logs_require_applied_write`;
  `cargo test -p harness-server --test checkpoint_recovery`;
  `cargo test -p harness-server --test task_db_rounds`.

### SP1716-T4 — Prove all six interleavings

- Owner: PostgreSQL verification owner.
- Dependencies: SP1716-T2 and SP1716-T3.
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009,
  B-010.
- Work: use an isolated PostgreSQL task store and deterministic stale snapshots
  to test `Applied`, `Superseded`, and `Conflict` for terminal replay, PR-only
  replay, checkpoint resume with PR writeback, checkpoint resume without PR
  writeback, no-checkpoint failure, and transient-retry failure. Capture
  tracing with a test-local subscriber and verify success wording is reachable
  only for applied writes.
- Done when: all eighteen interleaving cases assert their own UPDATE outcome,
  counters and logs exclude superseded/conflicting writes, different PR and
  nonmatching terminal evidence conflict, decode failures remain errors, and
  replay conflict preserves terminal evidence.
- Verify: run the focused `task_db::queries_recovery_tests` matrix with an
  isolated disposable `HARNESS_DATABASE_URL`, plus
  `cargo test -p harness-server --lib event_replay` and
  `cargo test -p harness-server --test checkpoint_recovery`.

### SP1716-T5 — Complete deterministic repository gates

- Owner: coordinator.
- Dependencies: SP1716-T4.
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009,
  B-010.
- Work: run formatting, focused tests, package checks, SpecRail validation, and
  exact path-scope review. Record PostgreSQL deferral explicitly when no
  isolated `HARNESS_DATABASE_URL` is available.
- Done when: all available local gates are green, the changed-file set stays
  inside the GH-1716 planned paths plus this task plan, and no schema,
  workflow-runtime, public HTTP API, or authorization surface changes.
- Verify:
  `cargo fmt --all`;
  `cargo fmt --all -- --check`;
  `cargo check -p harness-server --all-targets`;
  `python3 checks/check_workflow.py --repo .`;
  `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1716`;
  `git diff --name-only origin/main...HEAD`.

## Parallelization

Implementation is serial in this lane. The outcome contract and all six write
sites share recovery semantics and database state, so splitting writable work
would create overlapping ownership. Independent review may run read-only after
the implementation head is committed.

## Verification

- [x] Product invariant set is exactly B-001 through B-010.
- [x] Task coverage union is exactly B-001 through B-010.
- [x] All six version-guarded recovery UPDATE sites inspect affected rows.
- [x] Applied, superseded, and conflict interleavings are deterministic for all
      six sites against isolated PostgreSQL.
- [x] Recovery counters and success logs include only durable applied writes.
- [x] Replay conflict preserves terminal JSONL evidence and fails startup
      before checkpoint recovery.
- [x] Existing checkpoint, terminal replay, PR writeback, and transient-retry
      policy tests remain green.
- [x] Formatting, package checks, and SpecRail checks pass.
- [x] Exact diff scope contains only planned GH-1716 files.

## Handoff Notes

- This is a fail-closed data-integrity change: never infer supersession merely
  because the original eligibility predicate no longer matches.
- Do not blindly retry a lost CAS; a later startup begins from a fresh
  snapshot.
- PostgreSQL interleaving evidence requires an isolated disposable
  `HARNESS_DATABASE_URL`; if unavailable locally, report the exact deferral
  rather than substituting SQLite evidence.
- Final approval, push, PR creation, merge, and GitHub mutation remain outside
  this task plan.
