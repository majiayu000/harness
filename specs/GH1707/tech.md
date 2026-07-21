# Tech Spec

## Linked Issue

GH-1707

## Product Spec

See `specs/GH1707/product.md`.

<!-- specrail-planned-changes
{"issue":1707,"complete":true,"paths":["crates/harness-server/src/github_pr_snapshot.rs","crates/harness-server/src/github_pr_snapshot_tests.rs","crates/harness-server/src/intake/github_coverage_gate.rs","crates/harness-server/src/intake/github_coverage_recovery.rs","crates/harness-server/src/intake/github_coverage_recovery_test_support.rs","crates/harness-server/src/intake/github_coverage_recovery_tests.rs","crates/harness-server/src/intake/github_issue_links.rs","crates/harness-server/src/intake/mod.rs","crates/harness-server/src/task_executor/pr_detection.rs","crates/harness-server/src/task_executor/pr_detection_tests.rs","crates/harness-server/src/workflow_runtime_pr_feedback/pr_detection.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010"]}
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

1. Discover exact closing PR candidates from repository-qualified GitHub issue
   links and bounded REST fallback.
2. Fetch a complete server-owned snapshot for each candidate and reject
   incomplete, inconsistent, cross-repository, or closed-unmerged evidence.
3. Derive the target workflow state from PR state, checks, review decision,
   review threads, and merge facts.
4. Upsert the issue-bound workflow, PR binding, remote fact snapshot, and only
   the commands required for the derived state. Cancel stale implementation
   work before the coverage gate returns `Covered`.
5. Reuse stable workflow and command dedupe keys so repeated polls and restarts
   converge.
6. Propagate every lookup/completeness error to intake. Page-limit exhaustion
   and repeated pagination URLs are errors in both compiled PR-discovery
   implementations, not warning-plus-partial-result paths.

## Data Flow

`GitHub poll -> local coverage lookup -> authoritative issue/PR discovery ->
complete PR snapshot -> validate exact closing relation -> derive workflow
state -> transactional persistence/deduped commands -> Covered/Uncovered/error`.

Only a complete authoritative fact set can return `Covered` or `Uncovered`.
An external read failure returns an error, and intake skips dispatch for that
poll.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | `intake/github_coverage_gate.rs`, `github_coverage_recovery.rs` | `cargo test -p harness-server empty_store_recovers_ready_pr_and_stays_idempotent_after_restart` |
| B-002 | recovery persistence and PR snapshot modules | `cargo test -p harness-server recovery_maps_open_pr_facts` |
| B-003 | recovery state derivation | `cargo test -p harness-server recovery_maps_open_pr_facts` |
| B-004 | merged-state recovery | `cargo test -p harness-server merged_pr_closed_issue_recovers_terminal_coverage_without_agent_work` |
| B-005 | candidate validation | `cargo test -p harness-server closed_pr_is_uncovered_but_a_later_valid_pr_recovers_coverage` |
| B-006 | stable IDs and command dedupe | `cargo test -p harness-server empty_store_recovers_ready_pr_and_stays_idempotent_after_restart` |
| B-007 | stale-work cancellation | `cargo test -p harness-server recovery_maps_open_pr_facts` |
| B-008 | GitHub adapters and pagination guards | `cargo test -p harness-server github_lookup_failure_fails_closed_without_agent_work`; `cargo test -p harness-server existing_pr_lookup_fails_closed` |
| B-009 | closing-reference validation | `cargo test -p harness-server issue_linked_pr_without_closing_keyword_recovers_coverage` plus negative PR-detection tests |
| B-010 | recovery transaction/dedupe paths | restart test and `cargo test -p harness-server github_coverage_recovery` |

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
  repository-qualified closing evidence is mandatory.
- Compatibility: stale/cancelled local workflows must be reconciled without
  regressing newer or terminal state.
- Availability: GitHub outages intentionally defer intake instead of silently
  dispatching duplicate work.
- Performance: candidate pagination is bounded; exhausting the bound is a loud
  error rather than partial success.

## Test Plan

- [ ] Run every command in the Product-to-Test Mapping.
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
