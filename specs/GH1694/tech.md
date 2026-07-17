# Tech Spec

## Linked Issue

GH-1694

## Product Spec

See `specs/GH1694/product.md`.

## Current System

On base `8fa59c55e9335e163df433bd06a5aef097271fb9`,
`crates/harness-workflow/src/runtime/tests/worker_lifecycle.rs` contains 953
lines, 15 `#[tokio::test]` functions, and 145 assertion/expectation sites. It
is included directly into the `runtime::tests` module by:

```rust
include!("tests/worker_lifecycle.rs");
```

Lines 1-457 contain eight tests covering runtime-job claiming, PR lookup,
not-before scheduling, expired and unexpired leases, ready-work priority,
lease renewal, and baseline completion event recording. Line 458 is the blank
separator before the remaining group. Lines 459-953 contain seven contiguous
tests covering PR binding, malformed PR commands, implementation evidence,
closed issues, child completion propagation, and transactional parent
completion or rollback.

Because `include!` expands both sibling files into the same Rust module, the
completion-transition tests can move without adding a module segment to their
logical paths.

## Proposed Design

Keep exact-base lines 1-457 in
`runtime/tests/worker_lifecycle.rs`.

Create `runtime/tests/worker_completion_transitions.rs` from exact-base lines
458-953 in their current order. Moving the blank separator with the
completion-transition group prevents the retained file from ending with a
blank line, which the mandatory committed-whitespace check rejects.

In `runtime/tests.rs`, add:

```rust
include!("tests/worker_completion_transitions.rs");
```

immediately after the existing `include!("tests/worker_lifecycle.rs");`.

No imports or visibility changes are necessary because both include files
expand into the existing `runtime::tests` namespace.

## Retained Test Inventory

1. `runtime_worker_claims_one_job_once_and_records_events`
2. `runtime_store_get_instance_by_pr_filters_by_project_repo_and_pr`
3. `runtime_worker_skips_runtime_jobs_before_not_before`
4. `runtime_store_reclaims_expired_running_job`
5. `runtime_store_prioritizes_ready_work_over_other_activity_jobs`
6. `runtime_store_does_not_reclaim_unexpired_running_job`
7. `runtime_worker_renews_running_job_lease_until_completion`
8. `runtime_worker_records_completion_event_and_command_status`

## Moved Test Inventory

1. `runtime_worker_persists_bind_pr_payload_for_pr_open_transition`
2. `runtime_worker_blocks_invalid_inline_bind_pr_before_persisting_command`
3. `runtime_worker_blocks_implementation_success_without_pr_evidence`
4. `runtime_worker_finishes_closed_issue_success_without_pr`
5. `runtime_worker_propagates_pr_feedback_child_completion_to_parent`
6. `runtime_store_commits_parent_completion_event_decision_and_command`
7. `runtime_store_rolls_back_parent_completion_when_reducer_fails`

## Invariants

- Record the exact base SHA, 953-line count, ordered 15-test inventory, 145
  assertion/expectation sites, and split boundary before editing.
- Require the final retained file to equal exact-base lines 1-457.
- Require the new sibling file to equal exact-base lines 458-953.
- Require concatenating both final files to reproduce the exact base source.
- Preserve all 15 attributes, names, bodies, assertions, helper calls,
  artifacts, signals, JSON values, strings, and source bytes exactly once.
- Require every test path to remain unchanged.
- Require the only `runtime/tests.rs` change to be the adjacent include line.
- Require both formatted test files to contain no more than 800 lines and no
  file outside the approved three-file set to change.

## Affected Files

- `crates/harness-workflow/src/runtime/tests.rs`
- `crates/harness-workflow/src/runtime/tests/worker_lifecycle.rs`
- `crates/harness-workflow/src/runtime/tests/worker_completion_transitions.rs`

No production source, other test source, manifest, lockfile, configuration,
persistence, schema, or serialized-format file is expected to change.

## Data Flow

Workflow instances, runtime jobs, commands, activity results, artifacts,
signals, reducer decisions, events, leases, and transactions do not change.
Only the physical source file containing seven tests changes; Rust expands
both include files into the same existing test namespace.

## Alternatives Considered

- Leave the file unchanged: rejected because it remains above the hard ceiling
  and mixes bounded worker-lifecycle mechanics with issue-workflow completion
  transitions.
- Split `runtime_store.rs`: rejected for this cycle because that module mixes
  tests with database helpers, creating a higher-coupling split.
- Create a nested Rust module: rejected because it changes logical test paths
  without providing a behavioral or maintenance benefit.
- Move fewer final tests: rejected because it would fragment the cohesive
  completion-transition group and leave the retained file closer to the
  ceiling.
- Refactor helpers or production code simultaneously: rejected because it
  expands scope beyond a safe mechanical relocation.

## Risks

- Security: low; production source, authentication, secrets, process
  execution, input handling, and persistence implementation are unchanged.
- Compatibility: low; all logical test paths and filters remain unchanged.
- Performance: none expected outside negligible test compilation layout.
- Test integrity: low if exact retained/moved/concatenation comparisons and
  ordered inventory checks pass; all assertions and rejection cases remain
  mandatory.
- Maintenance: improved by separating completion transitions from core worker
  lifecycle mechanics.

## Test Plan

- [ ] Run the exact mechanical source checks below from the implementation
      worktree:

  ```bash
  BASE="$(git merge-base HEAD origin/main)"
  SOURCE=crates/harness-workflow/src/runtime/tests/worker_lifecycle.rs
  RETAINED=$SOURCE
  MOVED=crates/harness-workflow/src/runtime/tests/worker_completion_transitions.rs
  INCLUDES=crates/harness-workflow/src/runtime/tests.rs

  diff -u \
    <(git show "$BASE:$SOURCE" | sed -n '1,457p') \
    "$RETAINED"
  diff -u \
    <(git show "$BASE:$SOURCE" | sed -n '458,953p') \
    "$MOVED"
  cmp \
    <(cat "$RETAINED" "$MOVED") \
    <(git show "$BASE:$SOURCE")
  diff -u \
    <(git show "$BASE:$INCLUDES" | awk '
      { print }
      $0 == "include!(\"tests/worker_lifecycle.rs\");" {
        print "include!(\"tests/worker_completion_transitions.rs\");"
      }') \
    "$INCLUDES"
  diff -u \
    <(git show "$BASE:$SOURCE" |
      sed -nE 's/^async fn ([A-Za-z0-9_]+)\(.*/runtime::tests::\1/p') \
    <(sed -nE \
      's/^async fn ([A-Za-z0-9_]+)\(.*/runtime::tests::\1/p' \
      "$RETAINED" "$MOVED")
  ```

- [ ] Baseline and final `cargo check -p harness-workflow --all-targets`.
- [ ] Baseline and final
      `cargo test -p harness-workflow --lib runtime::tests::runtime_worker`,
      with the same 18 selected tests passing and unchanged paths.
- [ ] Baseline and final
      `cargo test -p harness-workflow --lib runtime::tests::runtime_store_`,
      with the same 12 selected tests passing and unchanged paths.
- [ ] Final database-independent `cargo test -p harness-workflow --lib` using
      a temporary empty `XDG_CONFIG_HOME` and unset Harness database variables.
- [ ] `cargo fmt --all -- --check`, `git diff --check`, line-count, exact
      retained file, exact moved file, exact concatenation, ordered inventory,
      namespace, include adjacency, 145-site inventory, and three-file scope
      checks.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` and workspace
      tests under the repository test environment.
- [ ] SpecRail global/packet/route checks, manual VibeGuard L1-L7 review,
      exact-head CI, review-thread audit, and required PR gate.

## Rollback Plan

Squash-revert the implementation PR. No data, schema, migration,
configuration, dependency, or operator rollback step is required.
