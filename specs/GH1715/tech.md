# Tech Spec

## Linked Issue

GH-1715

## Product Spec

See `specs/GH1715/product.md`.

<!-- specrail-planned-changes
{"issue":1715,"complete":true,"paths":["crates/harness-workflow/src/issue_lifecycle.rs","crates/harness-workflow/src/issue_workflow_store.rs","crates/harness-workflow/src/issue_workflow_store/maintenance.rs","crates/harness-workflow/src/issue_workflow_store/merge_approval.rs","crates/harness-workflow/src/issue_workflow_store/remote_facts.rs","crates/harness-workflow/src/issue_workflow_store_tests.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012"]}
-->

## Current System

- `crates/harness-workflow/src/issue_lifecycle.rs:13-52` declares 14 legacy
  lifecycle states and 16 lifecycle event kinds.
- `crates/harness-workflow/src/issue_lifecycle.rs:232-359` directly mutates
  lifecycle state and metadata for nearly every event. Only
  `HumanMergeApproved` checks its source state, and that branch warns and
  returns from a method whose return type is `()`.
- `crates/harness-workflow/src/issue_lifecycle.rs:417-496` tests the narrow
  merge-approval behavior but does not cover the state/event matrix or
  full-snapshot immutability on rejection.
- `crates/harness-workflow/src/issue_workflow_store.rs:214-448` exposes public
  event-specific mutation methods that call `apply_event`.
- `crates/harness-workflow/src/issue_workflow_store.rs:464-537` applies
  feedback-claim and release events, including stale placeholder recovery.
- `crates/harness-workflow/src/issue_workflow_store.rs:539-614` holds rows
  under transaction locks but accepts infallible mutation closures, so a
  transition error cannot currently abort through the helper boundary.
- `crates/harness-workflow/src/issue_workflow_store/remote_facts.rs:8-94`,
  `maintenance.rs:94-110`, and `merge_approval.rs:16-44` contain the remaining
  event-producing store methods.
- `crates/harness-workflow/src/issue_workflow_store_tests.rs:8-20` makes
  persistence coverage conditional on a PostgreSQL test database;
  `issue_workflow_store_tests.rs:416-511` covers merge approval but not general
  transition rollback.
- Production legacy calls remain reachable from
  `crates/harness-server/src/task_executor/run_task.rs:311-322`,
  `crates/harness-server/src/task_executor/implement_pipeline/outcome.rs:180-197`
  and `:501-524`, and
  `crates/harness-server/src/reconciliation_apply.rs:196-281`. These callers
  already consume `anyhow::Result`, so no planned server edit is required.

## Proposed Design

### Typed Transition Boundary

Add an `IssueLifecycleTransitionError` in `issue_lifecycle.rs` using the
workspace's existing `thiserror` dependency. The error records the workflow
identity, source state, event kind, and a stable reason category such as
`transition_not_allowed` or `binding_conflict`.

Change `IssueWorkflowInstance::apply_event` to return
`Result<(), IssueLifecycleTransitionError>`. It performs all state and
identity validation before mutating any instance field. On rejection, it must
not update `last_event`, `updated_at`, remote facts, task IDs, PR fields,
fallback state, or merge-attempt fields.

Represent the state/event allowlist in one auditable decision function rather
than distributed guards. Conditional cases may inspect existing instance
metadata and the incoming event:

- repeated PR detection requires the existing PR binding to match;
- repeated feedback scheduling requires the same task or a `claim:`
  placeholder;
- placeholder-backed feedback may be reclaimed;
- repeated merge start requires the same task/head attempt;
- `ImplementStarted` in `AddressingFeedback` updates execution metadata
  without collapsing the lifecycle to `Implementing`.

The allowlist implements the exact Transition Contract in `product.md`.
Its accepted result records both the target state and a closed metadata-effect
policy. All accepted events share only the three audit updates defined there;
event-specific code may mutate only the fields named for that row. This makes
same-state repetition testable rather than implicitly inheriting every current
assignment.

### Transactional Error Propagation

Change `update_issue`, `update_existing_issue`, and `update_by_pr` mutation
callbacks from `FnOnce(&mut IssueWorkflowInstance)` to a fallible callback
returning a result. Invoke the callback with `?` before `upsert_in_tx`.

Update all event-producing closures in the manifest to return the result from
`apply_event`. Metadata-only closures return `Ok(())`. Direct transactional
call sites such as feedback claiming, maintenance failure marking, and merge
approval use `?`.

Because validation finishes before mutation and persistence occurs only after
the callback succeeds, an illegal event rolls back the transaction and
preserves the prior row. Existing `SELECT ... FOR UPDATE` serialization
remains unchanged.

### Compatibility Boundaries

- Keep every enum variant, serde tag, event payload, workflow schema version,
  and SQL migration unchanged.
- Preserve the existing `IssueMergeApprovalOutcome` API. Its explicit
  wrong-state outcome remains the route-level contract; the lifecycle error is
  the lower-level fail-closed boundary.
- Do not change canonical workflow runtime reducers, validators, persistence,
  or state definitions.
- Do not change SpecRail `states.yaml` or interpret its process states as
  runtime states.
- Do not add a new dependency or repository/service layer.

## Data Flow

`public IssueWorkflowStore method -> begin transaction -> load workflow FOR
UPDATE -> construct event -> validate state/event and identity -> mutate
in-memory instance -> serialize/update row -> commit -> return updated
instance`.

For rejection:

`load workflow FOR UPDATE -> construct event -> typed transition error -> no
instance mutation -> no UPDATE -> transaction rollback on return -> caller
receives error`.

No external calls or new persistence records are introduced.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | centralized transition decision in `issue_lifecycle.rs` | `cargo test -p harness-workflow issue_lifecycle_transition_matrix --lib` evaluates all 224 pairs |
| B-002 | pre-mutation validation and transition error | `cargo test -p harness-workflow illegal_issue_lifecycle_transition_preserves_complete_snapshot --lib` |
| B-003 | terminal-state rows in the transition matrix | `cargo test -p harness-workflow terminal_issue_lifecycle_states_cannot_reopen --lib` |
| B-004 | transition matrix and closed metadata-effect policies | `cargo test -p harness-workflow issue_lifecycle_transition_matrix --lib`; `cargo test -p harness-workflow accepted_issue_lifecycle_events_mutate_only_declared_fields --lib` |
| B-005 | PR/task/merge identity guards | `cargo test -p harness-workflow repeated_issue_lifecycle_bindings_require_matching_identity --lib` |
| B-006 | placeholder conditional transitions | `cargo test -p harness-workflow feedback_claim_placeholder_transitions_remain_recoverable --lib` |
| B-007 | blocked terminal recovery rows | `cargo test -p harness-workflow blocked_issue_lifecycle_can_converge_to_terminal_state --lib` |
| B-008 | fallible store callbacks and transaction order | `HARNESS_DATABASE_URL=<isolated-test-db> cargo test -p harness-workflow --lib issue_workflow_store::tests::rejected_issue_lifecycle_store_update_rolls_back -- --ignored --exact` must execute a required-DB fixture rather than the optional helper |
| B-009 | typed error propagation from store methods | `cargo test -p harness-workflow issue_workflow_store_reports_illegal_transition --lib` |
| B-010 | serde and existing valid store behavior | `cargo test -p harness-workflow issue_lifecycle --lib`; `cargo test -p harness-workflow issue_workflow_store --lib` |
| B-011 | row-lock race coverage | `HARNESS_DATABASE_URL=<isolated-test-db> cargo test -p harness-workflow --lib issue_workflow_store::tests::concurrent_valid_and_invalid_issue_transitions_preserve_winner -- --ignored --exact` must execute a required-DB fixture rather than the optional helper |
| B-012 | manifest scope and workspace compatibility | `git diff --name-only origin/main...HEAD`; `cargo check -p harness-workflow --all-targets` |

## Alternatives Considered

- Validate only terminal source states: rejected because nonterminal regressions
  and feedback-stage collapse would remain possible.
- Keep warning-and-ignore behavior: rejected because callers cannot distinguish
  rejection from success and persistence cannot prove rollback.
- Add guards independently to each store method: rejected because direct
  lifecycle callers and future store methods could bypass a distributed
  contract.
- Reuse the canonical workflow runtime validator: rejected because this legacy
  model has different states, events, placeholder semantics, and retirement
  boundaries.
- Merge the Rust lifecycle models or import SpecRail states: rejected as
  unrelated architecture scope.

## Risks

- Security: no authorization or secret-handling path changes. Error text must
  not include tokens or raw external payloads.
- Logic: an incomplete allowlist could reject a legitimate recovery path.
  Exhaustive matrix coverage and explicit placeholder tests are mandatory.
- Data integrity: validation after partial mutation would leak changed fields
  to callers even without persistence. Validate first and compare complete
  snapshots in tests.
- Compatibility: latent callers may depend on permissive transitions.
  Rejections must be visible so those ordering defects can be corrected rather
  than silently accepted.
- Concurrency: changing callback signatures must not move validation outside
  the existing row-lock transaction.
- Maintenance: duplicated guards in store methods could drift from the central
  matrix; the lifecycle contract remains the single source.
- Performance: the decision is an in-memory enum match and adds no I/O or
  asymptotic cost.

## Test Plan

- [ ] Add a table-driven unit test for every 14 × 16 state/event pair.
- [ ] Add full-snapshot immutability assertions for each rejection class.
- [ ] Add positive and negative identity tests for repeated PR detection,
      feedback task binding, placeholder reclaim, and merge-start repetition.
- [ ] Add field-diff assertions for every accepted metadata-effect policy,
      including audit-only terminal repetition and preservation of unlisted
      fields.
- [ ] Retain and update existing merge approval and feedback recovery tests.
- [ ] Add a required-DB test helper for the two new persistence tests. It reads
      `HARNESS_DATABASE_URL`, validates an isolated test database through the
      existing database-safety helpers, calls
      `IssueWorkflowStore::open_with_database_url`, and errors rather than
      returning `Ok(None)` when configuration/open fails. Keep the existing
      optional helper only for legacy tests.
- [ ] With an isolated `HARNESS_DATABASE_URL`, run the two named ignored tests
      with `--ignored --exact`; their output must prove store rejection performs
      no persisted update and a valid/invalid race preserves the serialized
      winner. A run without executed database assertions is not evidence.
- [ ] Run `cargo test -p harness-workflow issue_lifecycle --lib`.
- [ ] Run `cargo test -p harness-workflow issue_workflow_store --lib`.
- [ ] Run `cargo check -p harness-workflow --all-targets`.
- [ ] Before commit, run `cargo fmt --all` and
      `cargo fmt --all -- --check`.
- [ ] Before push, run
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1715`.
- [ ] Confirm the implementation diff contains only the six paths in the
      planned-changes manifest.

## Rollback Plan

Revert the implementation commit. No database rollback, data rewrite, or
feature-flag operation is required because the change adds no schema or stored
representation. Reversion restores permissive legacy transitions and therefore
also restores the terminal-state corruption risk; it should be used only if a
valid production transition was omitted and cannot be corrected immediately.
