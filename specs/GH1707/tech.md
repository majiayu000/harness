# Tech Spec

## Linked Issue

GH-1707

## Product Spec

See `specs/GH1707/product.md`.

<!-- specrail-planned-changes
{"issue":1707,"complete":true,"paths":["crates/harness-server/src/github_pr_snapshot.rs","crates/harness-server/src/github_pr_snapshot_tests.rs","crates/harness-server/src/intake/github_coverage_gate.rs","crates/harness-server/src/intake/github_coverage_recovery.rs","crates/harness-server/src/intake/github_coverage_recovery_tests.rs","crates/harness-server/src/intake/github_coverage_recovery_tests/atomic.rs","crates/harness-server/src/intake/github_coverage_recovery_tests/support.rs","crates/harness-server/src/intake/github_issue_links.rs","crates/harness-server/src/intake/mod.rs","crates/harness-server/src/task_executor/pr_detection.rs","crates/harness-server/src/task_executor/pr_detection_tests.rs","crates/harness-server/src/workflow_runtime_pr_feedback/pr_detection.rs","crates/harness-server/src/workflow_runtime_worker/child_workflow.rs","crates/harness-server/src/workflow_runtime_worker/child_workflow_non_issue.rs","crates/harness-workflow/src/runtime/remote_facts.rs","crates/harness-workflow/src/runtime/store.rs","crates/harness-workflow/src/runtime/store/commands.rs","crates/harness-workflow/src/runtime/store/coverage_recovery.rs","crates/harness-workflow/src/runtime/store/runtime_job_state.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012"]}
-->

## Current System

- `crates/harness-server/src/intake/mod.rs:251` calls the coverage gate before
  direct workflow-runtime dispatch.
- `crates/harness-server/src/intake/github_coverage_gate.rs:103` checks local
  runtime coverage, but the base implementation does not reconstruct complete
  coverage from current GitHub PR facts after persistence loss.
- `crates/harness-server/src/workflow_runtime_pr_feedback/pr_detection.rs`
  already owns GitHub PR discovery helpers, while the still-compiled legacy
  `task_executor/pr_detection.rs` contains the same pagination contract.
- Server-owned PR snapshots and workflow-runtime commands are the durable
  evidence surfaces that recovery must populate atomically and idempotently.

## Proposed Design

1. Page the GitHub GraphQL `issue.closedByPullRequestsReferences` connection to
   completion. This repository-qualified connection is the only source of
   closing candidates; do not run a REST keyword fallback or require a closing
   keyword in the linked PR body.
2. Fetch a complete server-owned snapshot for every same-repository candidate.
   Any transport, parsing, pagination, or completeness error aborts the poll;
   do not choose from a partial candidate set.
3. Classify and select candidates with a stable comparison key, never API
   order. Eligible merged candidates have precedence over eligible active
   candidates even while the linked issue remains `OPEN`; that combination is
   treated as GitHub propagation lag and fails safe to terminal coverage.
   Closed-unmerged candidates are always ineligible. Within the selected class,
   choose the highest PR number.
4. Derive the target workflow state from PR state, checks, review decision,
   review threads, and merge facts using the exact matrix below.
5. Atomically compare-and-swap the issue-bound workflow, selected PR fact,
   recovery event/decision, stale-command cancellation, and only the commands
   required for the derived state. A losing recovery performs no cancellation
   or partial fact/binding write. A cancelled deterministic quality-gate
   command is reactivated and can enqueue a fresh job after a terminal prior
   job.
6. Reuse stable workflow and command dedupe keys so repeated polls and restarts
   converge.
7. Propagate every lookup/completeness error to intake. Page-limit exhaustion
   and repeated pagination URLs are errors in both compiled PR-discovery
   implementations, not warning-plus-partial-result paths.
8. Preserve the issue author's trust class through recovery and child-workflow
   creation. If either current intake or durable workflow metadata identifies
   a non-collaborator, recovery must retain `non_collaborator`; missing or
   malformed recovery trust evidence must never elevate execution to trusted.

## Recovery State Matrix

| Complete GitHub facts | Recovery result |
| --- | --- |
| Open PR waiting for checks or mergeability | `pr_open` |
| Open PR requiring CI or review repair | `awaiting_feedback` |
| Open PR with ready snapshot | `quality_gate_pending` plus one deduplicated quality-gate child command |
| Successful quality-gate child completion from `quality_gate_pending` | `ready_to_merge` |
| Authoritative linked merged PR and closed issue | terminal `done` with merge evidence |
| Authoritative linked merged PR and open issue | terminal `done` with merge evidence; issue state is treated as propagation lag |
| Closed-unmerged PR | ineligible; not durable coverage |

Recovery must never map a ready snapshot directly to `ready_to_merge`.

## Data Flow

`GitHub poll -> local coverage lookup -> complete authoritative GraphQL issue
links -> complete snapshots for all same-repository candidates -> deterministic
class/PR-number arbitration -> derive workflow state -> transactional
persistence/deduped commands -> Covered/Uncovered/error`.

Only a complete authoritative fact set can return `Covered` or `Uncovered`.
An external read failure returns an error, and intake skips dispatch for that
poll.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | `intake/github_coverage_gate.rs`, `github_coverage_recovery.rs` | `cargo test -p harness-server empty_store_recovers_ready_pr_and_stays_idempotent_after_restart` |
| B-002 | recovery persistence and PR snapshot modules | `cargo test -p harness-server recovery_maps_open_pr_facts` |
| B-003 | recovery state derivation and quality-gate parent propagation | `cargo test -p harness-server recovery_maps_open_pr_facts`; `cargo test -p harness-workflow runtime_completion_reducer_marks_issue_pr_ready_after_quality_gate_pass` |
| B-004 | merged-state recovery under settled and lagging issue state | `cargo test -p harness-server merged_pr_and_closed_issue_recover_terminal_coverage_without_agent_work`; `cargo test -p harness-server merged_pr_and_open_issue_recover_terminal_coverage_without_agent_work` |
| B-005 | candidate eligibility | `cargo test -p harness-server closed_pr_is_uncovered_but_a_later_valid_pr_recovers_coverage` |
| B-006 | stable IDs and command dedupe | `cargo test -p harness-server empty_store_recovers_ready_pr_and_stays_idempotent_after_restart` |
| B-007 | stale-work cancellation | `cargo test -p harness-server recovery_maps_open_pr_facts` |
| B-008 | GitHub adapters and pagination guards | `cargo test -p harness-server github_lookup_failure_fails_closed_without_agent_work`; `cargo test -p harness-server existing_pr_lookup_fails_closed` |
| B-009 | GraphQL issue-link authority | `cargo test -p harness-server issue_linked_pr_without_closing_keyword_recovers_coverage`; negative test `rest_keyword_without_authoritative_issue_link_is_uncovered` |
| B-010 | deterministic arbitration and recovery transaction/dedupe paths | `cargo test -p harness-server multiple_closing_pr_candidates_are_selected_deterministically`; barrier-controlled `cargo test -p harness-server concurrent_recovery_attempts_converge_on_one_binding_and_command_set` |

## Alternatives Considered

- Trust local workflow rows only: rejected because those rows are precisely the
  state lost in the incident.
- Treat lookup failures as uncovered: rejected because that spends model work
  under uncertainty and recreates the production defect.
- Persist only a lightweight “covered” marker: rejected because downstream PR
  feedback and terminal reconciliation need auditable PR facts and bindings.
- Search branch names or PR titles: rejected because they are non-authoritative
  and create false ownership.

## Risks

- Security: GitHub tokens remain read-only for evidence collection; errors and
  redaction must not expose credentials.
- Data integrity: incorrect candidate selection could bind the wrong PR; exact
  repository-qualified GraphQL closing evidence and deterministic arbitration
  are mandatory.
- Compatibility: stale/cancelled local workflows must be reconciled without
  regressing newer or terminal state.
- Availability: GitHub outages intentionally defer intake instead of silently
  dispatching duplicate work.
- Performance: candidate pagination is bounded; exhausting the bound is a loud
  error rather than partial success.

## Test Plan

- [ ] Run every command in the Product-to-Test Mapping.
- [ ] Run the multi-candidate test with the same candidates in forward and
      reverse GraphQL order and assert the same selected PR, including a
      merged candidate while the issue remains open.
- [ ] Run merged-plus-open and merged-plus-closed terminal recovery tests and
      assert `done`, merge evidence, and zero implementation or repair work.
- [ ] Run the concurrency test with an explicit barrier that holds two
      recovery attempts after candidate selection and releases both to race the
      persistence boundary; assert one binding, one state, and one deduplicated
      command set.
- [ ] Race recovery against an ordinary workflow transition and assert a losing
      recovery cannot cancel the winner's command or overwrite its state/data.
- [ ] Cancel a recovered deterministic quality-gate command both before and
      after a runtime job exists; recovery reactivates it and dispatches one
      fresh job while retaining terminal job history.
- [ ] Recover a non-collaborator issue and assert both the parent and its
      quality-gate/feedback child commands resolve to non-collaborator
      isolation.
- [ ] Run `cargo check -p harness-server --all-targets`.
- [ ] Run `cargo fmt --all -- --check` and
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run PostgreSQL-backed `harness-server` tests with an isolated
      `HARNESS_DATABASE_URL`, or rely on exact-head CI when no disposable local
      database is configured.
- [ ] Run `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1707`.
- [ ] Collect exact-head CI, Gemini review, independent reviewer evidence,
      GraphQL review-thread state, and SpecRail PR-gate evidence.

## Rollback Plan

Squash-revert the implementation PR. No schema rollback or destructive data
operation is required. During rollback, disable affected repository intake if
necessary rather than accepting duplicate dispatch under uncertain coverage.
