# Task Plan

## Linked Issue

GH-1715

## Spec Packet

- Product: `specs/GH1715/product.md`
- Tech: `specs/GH1715/tech.md`

## Implementation Tasks

- [ ] `SP1715-T1` Owner: Codex implementation agent | Done when: the complete legacy lifecycle transition contract rejects every unlisted pair before mutation, preserves declared idempotent and recovery paths, and focused lifecycle tests pass | Verify: commands in SP1715-T1 below | Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-010
- [ ] `SP1715-T2` Owner: Codex implementation agent | Done when: every scoped store mutation propagates typed transition failures while holding the existing row lock, applies store metadata only after validation, and rollback, batch, merge-approval, and race tests pass | Verify: commands in SP1715-T2 below | Covers: B-002, B-004, B-005, B-006, B-007, B-008, B-009, B-010, B-011
- [ ] `SP1715-T3` Owner: Codex implementation agent | Done when: the packet reconciles PR #1725's legacy `task_executor` deletion, preserves store-level fallback validation, and neither the manifest nor implementation revives a deleted path | Verify: commands in SP1715-T3 below | Covers: B-009, B-010, B-012
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
  - allow `PrDetected` to replace a prior stage task before `PrOpen`, but require
    compatible task and PR bindings for repeated `PrDetected` from `PrOpen`;
  - update direct lifecycle callers and unit tests to consume the result;
  - move lifecycle transition coverage into the searched-first companion
    `issue_lifecycle_tests.rs`, and add table, complete-snapshot, metadata-diff,
    terminal, binding, placeholder, and blocked-recovery tests.
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
  - `cargo test -p harness-workflow pr_detected_rebinds_stage_task_before_pr_open --lib`;
  - `cargo test -p harness-workflow repeated_pr_detected_requires_compatible_task_and_pr_bindings --lib`;
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

### SP1715-T3 — Reconcile the upstream legacy executor deletion

- Owner: Codex implementation agent.
- Dependencies: PR #1725 merged into the implementation branch; SP1715-T2.
- Covers: B-009, B-010, B-012.
- Work:
  - remove the deleted legacy executor source and test paths from the planned
    changes manifest and all acceptance/verification requirements;
  - retain store-level `record_ready_to_merge_with_fallback` validation,
    compatible retry behavior, and error propagation;
  - keep the upstream deletion authoritative and do not recreate or modify any
    legacy `task_executor` path.
- Done when:
  - the manifest contains only the seven workflow implementation paths;
  - no server executor command or required-DB test remains in this packet;
  - the implementation diff does not revive a deleted `task_executor` file;
  - store metadata validation remains covered by SP1715-T2.
- Verify:
  - `python3 -c 'import json, re; from pathlib import Path; text = Path("specs/GH1715/tech.md").read_text(); data = json.loads(re.search(r"<!-- specrail-planned-changes\\s*(\\{.*?\\})\\s*-->", text, re.S).group(1)); assert len(data["paths"]) == 7; assert not any("/task_executor/" in path for path in data["paths"])'`;
  - `test ! -e crates/harness-server/src/task_executor/review_loop/flow.rs`;
  - `test ! -e crates/harness-server/src/task_executor/review_loop_wait_budget_tests.rs`;
  - `git diff --quiet origin/main...HEAD -- crates/harness-server/src/task_executor`.

### SP1715-T4 — Prove compatibility, scope, and repository readiness

- Owner: Codex implementation agent; human review remains authoritative.
- Dependencies: SP1715-T1, SP1715-T2, SP1715-T3.
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009,
  B-010, B-011, B-012.
- Work:
  - run all focused verification at the final head, including the four
    required-DB tests against an isolated database;
  - run workflow package tests, workspace checks, formatting, Clippy, and
    SpecRail deterministic checks;
  - confirm the diff is restricted to this task plan and the seven paths in
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
  - the four exact required-DB commands from SP1715-T2;
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
and SP1715-T3 reconciles the upstream deletion against that finalized store
behavior. Execute them serially in one worktree to preserve transaction
ordering and avoid overlapping ownership. Read-only review may run after
SP1715-T4 without modifying this task plan.

## Verification

- [ ] The union of `Covers:` fields is exactly B-001 through B-012.
- [ ] The 14 × 16 transition matrix and complete-snapshot rejection tests pass.
- [ ] All focused workflow tests pass.
- [ ] All four required-DB tests execute against an isolated PostgreSQL database
      and pass; none use an optional skip path.
- [ ] Workspace check, formatting, Clippy, and both SpecRail checks pass.
- [ ] The final diff is restricted to `specs/GH1715/tasks.md` and the seven
      implementation paths declared in `tech.md`.

## Handoff Notes

- Preserve the current `SELECT ... FOR UPDATE` transaction boundary and invoke
  fallible callbacks before persistence.
- Validate the entire event and every binding before mutating any workflow
  field. Rejection must preserve the complete in-memory and persisted snapshot.
- Keep feedback claiming batch-atomic; never log-and-continue an illegal
  candidate.
- Keep fallback validation inside the lifecycle store and preserve the first
  compatible snapshot across retries.
- Do not revive legacy `task_executor` files removed by PR #1725.
- Keep all enum variants, serde tags, payloads, schemas, migrations, canonical
  workflow-runtime code, and SpecRail process files unchanged.
- PostgreSQL commands require an isolated disposable database through
  `HARNESS_DATABASE_URL`; absence of that database blocks those acceptance
  claims.
