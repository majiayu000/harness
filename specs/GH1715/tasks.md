# Task Plan

## Linked Issue

GH-1715

## Spec Packet

- Product: `specs/GH1715/product.md`
- Tech: `specs/GH1715/tech.md`

## Implementation Tasks

- [ ] `SP1715-T1` Owner: Codex implementation agent | Done when: the complete legacy lifecycle transition contract rejects every unlisted pair before mutation, preserves declared idempotent and recovery paths, and focused lifecycle tests pass | Verify: commands in SP1715-T1 below | Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-010
- [ ] `SP1715-T2` Owner: Codex implementation agent | Done when: every scoped store mutation propagates typed transition failures while holding the existing row lock, applies store metadata only after validation, and rollback, batch, merge-approval, and race tests pass | Verify: commands in SP1715-T2 below | Covers: B-002, B-004, B-005, B-006, B-007, B-008, B-009, B-010, B-011
- [ ] `SP1715-T3` Owner: Codex implementation agent | Done when: Tier-C records lifecycle readiness before task completion, propagates rejection, preserves the first compatible fallback snapshot on retry, and both required-DB server tests pass | Verify: commands in SP1715-T3 below | Covers: B-002, B-005, B-008, B-009, B-010
- [ ] `SP1715-T4` Owner: Codex implementation agent | Done when: all focused, package, workspace, formatting, lint, SpecRail, and manifest-scope gates pass with fresh evidence | Verify: commands in SP1715-T4 below | Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010, B-011, B-012

### SP1715-T1 — Enforce the complete lifecycle transition contract

- Owner: Codex implementation agent.
- Dependencies: approved `product.md` and `tech.md`; clean implementation
  worktree based on current `origin/main`.
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-010.
- Work:
  - add a typed lifecycle transition error carrying workflow identity, source
    state, event kind, and stable rejection category;
  - centralize all 224 state/event decisions in one fail-closed transition
    function that validates identities before mutating any field;
  - implement the closed event-specific metadata effects from the product
    contract, including audit-only terminal repetitions, placeholder reclaim,
    blocked-to-terminal convergence, and same-task/head/attempt checks;
  - update direct lifecycle callers and unit tests to consume the result;
  - add table, complete-snapshot, metadata-diff, terminal, binding, placeholder,
    and blocked-recovery tests.
- Done when:
  - every accepted transition matches the product matrix and every unlisted
    transition returns the typed error;
  - rejected events leave the complete in-memory snapshot unchanged;
  - lifecycle enum variants, event payloads, serde tags, and schema version are
    unchanged;
  - all focused lifecycle tests pass.
- Verify:
  - `cargo test -p harness-workflow issue_lifecycle_transition_matrix --lib`;
  - `cargo test -p harness-workflow illegal_issue_lifecycle_transition_preserves_complete_snapshot --lib`;
  - `cargo test -p harness-workflow terminal_issue_lifecycle_states_cannot_reopen --lib`;
  - `cargo test -p harness-workflow accepted_issue_lifecycle_events_mutate_only_declared_fields --lib`;
  - `cargo test -p harness-workflow repeated_issue_lifecycle_bindings_require_matching_identity --lib`;
  - `cargo test -p harness-workflow feedback_claim_placeholder_transitions_remain_recoverable --lib`;
  - `cargo test -p harness-workflow blocked_issue_lifecycle_can_converge_to_terminal_state --lib`;
  - `cargo test -p harness-workflow issue_lifecycle --lib`.

### SP1715-T2 — Propagate transition errors through locked store updates

- Owner: Codex implementation agent.
- Dependencies: SP1715-T1.
- Covers: B-002, B-004, B-005, B-006, B-007, B-008, B-009, B-010, B-011.
- Work:
  - make `update_issue`, `update_existing_issue`, and `update_by_pr` accept
    fallible callbacks without moving validation outside the row-lock
    transaction;
  - propagate every scoped event application error and return `Ok(())` only
    from metadata-only closures;
  - apply scheduling metadata and compatible Tier-C fallback snapshots only
    after successful lifecycle validation;
  - retain batch-atomic feedback claiming and align merge approval so
    `ReadyToMerge` and repeated `Done` return `Applied`, while illegal source
    states return the typed transition error;
  - add required-DB rollback, metadata, batch-abort, and serialized-race tests
    that fail rather than skip when `HARNESS_DATABASE_URL` is unavailable.
- Done when:
  - a rejected update performs no `UPDATE`, commits no partial snapshot, and
    reaches the caller as an error;
  - one illegal feedback candidate aborts the whole batch;
  - compatible store retries are idempotent and conflicting bindings fail;
  - all focused in-memory tests pass and all four required-DB tests execute
    against an isolated PostgreSQL database.
- Verify:
  - `cargo test -p harness-workflow issue_workflow_store_reports_illegal_transition --lib`;
  - `cargo test -p harness-workflow merge_approval_wrong_state_returns_transition_error --lib`;
  - `cargo test -p harness-workflow repeated_merge_approval_from_done_is_applied_idempotently --lib`;
  - `cargo test -p harness-workflow issue_workflow_store --lib`;
  - `HARNESS_DATABASE_URL=<isolated-test-db> cargo test -p harness-workflow --lib issue_workflow_store::tests::issue_workflow_store_metadata_requires_valid_transition -- --ignored --exact`;
  - `HARNESS_DATABASE_URL=<isolated-test-db> cargo test -p harness-workflow --lib issue_workflow_store::tests::rejected_issue_lifecycle_store_update_rolls_back -- --ignored --exact`;
  - `HARNESS_DATABASE_URL=<isolated-test-db> cargo test -p harness-workflow --lib issue_workflow_store::tests::feedback_claim_batch_aborts_on_illegal_transition -- --ignored --exact`;
  - `HARNESS_DATABASE_URL=<isolated-test-db> cargo test -p harness-workflow --lib issue_workflow_store::tests::concurrent_valid_and_invalid_issue_transitions_preserve_winner -- --ignored --exact`.

### SP1715-T3 — Make Tier-C completion fail closed

- Owner: Codex implementation agent.
- Dependencies: SP1715-T2.
- Covers: B-002, B-005, B-008, B-009, B-010.
- Work:
  - move `record_ready_to_merge_with_fallback` before the task completion
    mutation and propagate its result;
  - ensure a lifecycle rejection leaves task status, task rounds, runtime
    feedback, and completion events unchanged;
  - preserve the first fallback snapshot, including `activated_at`, across a
    lifecycle-success/task-store-failure retry with the same logical identity;
  - add non-skipping required-DB server tests for rejection and retry
    convergence.
- Done when:
  - no completion-shaped evidence is recorded after lifecycle rejection;
  - retry after a task-store failure converges to exactly one task round,
    runtime feedback result, and completion event;
  - the original compatible fallback snapshot is preserved;
  - focused server tests and package check pass.
- Verify:
  - `HARNESS_DATABASE_URL=<isolated-test-db> cargo test -p harness-server --lib task_executor::review_loop_wait_budget_tests::tier_c_lifecycle_rejection_records_no_completion_evidence -- --ignored --exact`;
  - `HARNESS_DATABASE_URL=<isolated-test-db> cargo test -p harness-server --lib task_executor::review_loop_wait_budget_tests::tier_c_task_store_failure_retry_records_completion_once -- --ignored --exact`;
  - `cargo check -p harness-server --all-targets`;
  - `python3 -c 'from pathlib import Path; text = Path("crates/harness-server/src/task_executor/review_loop/flow.rs").read_text(); lifecycle = text.index("record_ready_to_merge_with_fallback"); assert lifecycle < text.index("s.status = TaskStatus::Done", lifecycle)'`.

### SP1715-T4 — Prove compatibility, scope, and repository readiness

- Owner: Codex implementation agent; human review remains authoritative.
- Dependencies: SP1715-T1, SP1715-T2, SP1715-T3.
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009,
  B-010, B-011, B-012.
- Work:
  - run all focused verification at the final head, including the six
    required-DB tests against an isolated database;
  - run workflow and server package tests, workspace checks, formatting,
    Clippy, and SpecRail deterministic checks;
  - confirm the diff is restricted to this task plan and the eight paths in
    the planned-changes manifest;
  - record unavailable external test infrastructure as a blocker rather than
    treating skipped tests as evidence.
- Done when:
  - all 12 product invariants have fresh deterministic evidence;
  - no lifecycle wire/schema or canonical workflow-runtime surface changes;
  - all commands pass, or an infrastructure blocker is reported with the
    unexecuted command clearly identified;
  - the final diff contains no unassigned path.
- Verify:
  - `cargo test -p harness-workflow issue_lifecycle --lib`;
  - `cargo test -p harness-workflow issue_workflow_store --lib`;
  - the six exact required-DB commands from SP1715-T2 and SP1715-T3;
  - `cargo check --workspace --all-targets`;
  - `cargo fmt --all`;
  - `cargo fmt --all -- --check`;
  - `cargo clippy --workspace --all-targets -- -D warnings`;
  - `python3 checks/check_workflow.py --repo .`;
  - `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1715`;
  - `git diff --name-only origin/main...HEAD`.

## Parallelization

No parallel writable lanes are planned. SP1715-T1 changes the result contract
consumed by every store method, SP1715-T2 updates those shared store helpers,
and SP1715-T3 depends on the finalized store behavior. Execute them serially in
one worktree to preserve transaction ordering and avoid overlapping ownership.
Read-only review may run after SP1715-T4 without modifying this task plan.

## Verification

- [ ] The union of `Covers:` fields is exactly B-001 through B-012.
- [ ] The 14 × 16 transition matrix and complete-snapshot rejection tests pass.
- [ ] All focused workflow and server tests pass.
- [ ] All six required-DB tests execute against an isolated PostgreSQL database
      and pass; none use an optional skip path.
- [ ] Workspace check, formatting, Clippy, and both SpecRail checks pass.
- [ ] The final diff is restricted to `specs/GH1715/tasks.md` and the eight
      implementation paths declared in `tech.md`.

## Handoff Notes

- Preserve the current `SELECT ... FOR UPDATE` transaction boundary and invoke
  fallible callbacks before persistence.
- Validate the entire event and every binding before mutating any workflow
  field. Rejection must preserve the complete in-memory and persisted snapshot.
- Keep feedback claiming batch-atomic; never log-and-continue an illegal
  candidate.
- The lifecycle and task stores do not share a transaction. The fail-closed
  order is lifecycle readiness first, then task completion and runtime effects.
- Keep all enum variants, serde tags, payloads, schemas, migrations, canonical
  workflow-runtime code, and SpecRail process files unchanged.
- PostgreSQL commands require an isolated disposable database through
  `HARNESS_DATABASE_URL`; absence of that database blocks those acceptance
  claims.
