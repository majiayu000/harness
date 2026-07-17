# Tech Spec

## Linked Issue

GH-1690

## Product Spec

See `specs/GH1690/product.md`.

## Current System

On base `888c64a48995b4fc89532b06434b562960c7f4b9`,
`crates/harness-workflow/src/runtime/tests/completion_reducer_quality.rs`
contains 896 lines and 23 `#[test]` functions. It is included directly into
the `runtime::tests` module by:

```rust
include!("tests/completion_reducer_quality.rs");
```

Lines 1-498 contain 13 tests covering PR-feedback decisions, invalid or
unknown structured decisions, stale completion handling, child start
acknowledgements, and already-terminal workflows. Lines 499-896 contain 10
contiguous tests covering quality-gate dispatch, successful completion,
issue-PR readiness, missing or conflicting evidence, invalid structured
decisions, and transient retries.

Because `include!` expands both sibling files into the same Rust module, the
quality-gate tests can move without adding a module segment to their logical
paths.

## Proposed Design

Keep exact-base lines 1-498 in
`runtime/tests/completion_reducer_quality.rs`.

Create
`runtime/tests/completion_reducer_quality_gate.rs` from exact-base lines
499-896 in their current order.

In `runtime/tests.rs`, add:

```rust
include!("tests/completion_reducer_quality_gate.rs");
```

immediately after the existing
`include!("tests/completion_reducer_quality.rs");`.

No imports or visibility changes are necessary because both include files
expand into the existing `runtime::tests` namespace.

## Retained Test Inventory

1. `runtime_completion_reducer_maps_pr_feedback_sweep_signal_to_address_command`
2. `runtime_completion_reducer_maps_pr_feedback_child_signal_to_parent_decision`
3. `runtime_completion_reducer_updates_pr_feedback_child_from_inspection_signal`
4. `runtime_completion_reducer_uses_pr_feedback_child_signal_when_structured_decision_is_invalid`
5. `runtime_completion_reducer_prefers_blocking_pr_feedback_over_ready_signal`
6. `runtime_completion_reducer_blocks_invalid_structured_workflow_decision`
7. `runtime_completion_reducer_blocks_unknown_success_without_structured_decision`
8. `runtime_completion_reducer_ignores_stale_pr_feedback_sweep_after_parent_advances`
9. `runtime_completion_reducer_ignores_stale_pr_feedback_child_after_parent_advances`
10. `runtime_completion_reducer_ignores_stale_local_review_after_pass_advances_parent`
11. `runtime_completion_reducer_ignores_stale_local_review_after_changes_advance_parent`
12. `runtime_completion_reducer_ignores_child_workflow_start_ack_without_parent_decision`
13. `runtime_completion_reducer_ignores_success_for_already_terminal_workflow`

## Moved Test Inventory

1. `quality_gate_run_decision_starts_runtime_activity`
2. `runtime_completion_reducer_passes_quality_gate_after_success`
3. `runtime_completion_reducer_marks_issue_pr_ready_after_quality_gate_pass`
4. `runtime_completion_reducer_blocks_issue_pr_quality_gate_without_validation`
5. `runtime_completion_reducer_blocks_quality_gate_success_without_status_signal`
6. `runtime_completion_reducer_blocks_quality_gate_success_without_validation_evidence`
7. `runtime_completion_reducer_blocks_quality_gate_success_with_conflicting_status_signals`
8. `runtime_completion_reducer_blocks_quality_gate_structured_pass_without_contract_evidence`
9. `runtime_completion_reducer_blocks_quality_gate_invalid_structured_decision_with_contract_evidence`
10. `runtime_completion_reducer_retries_quality_gate_transient_failure`

## Invariants

- Record the exact base SHA, line count, ordered 23-test inventory, attributes,
  and split boundary before editing.
- Require the final retained file to equal exact-base lines 1-498.
- Require the new sibling file to equal exact-base lines 499-896.
- Preserve all 23 attributes, names, bodies, assertions, helper calls,
  artifacts, signals, JSON values, strings, and non-whitespace Rust tokens
  exactly once.
- Require every test path to remain unchanged.
- Require the only `runtime/tests.rs` change to be the adjacent include line.
- Require both formatted test files to contain no more than 800 lines and no
  file outside the approved three-file set to change.

## Affected Files

- `crates/harness-workflow/src/runtime/tests.rs`
- `crates/harness-workflow/src/runtime/tests/completion_reducer_quality.rs`
- `crates/harness-workflow/src/runtime/tests/completion_reducer_quality_gate.rs`

No production source, other test source, manifest, lockfile, configuration,
persistence, schema, or serialized-format file is expected to change.

## Data Flow

Workflow instances, activity results, artifacts, signals, reducer decisions,
quality-gate commands, retry decisions, and validations do not change. Only the
physical source file containing 10 tests changes; Rust expands both include
files into the same existing test namespace.

## Alternatives Considered

- Leave the file unchanged: rejected because it remains above the hard ceiling
  and mixes a bounded quality-gate group with PR-feedback and stale-completion
  tests.
- Create a nested Rust module: rejected because it changes logical test paths
  without providing a behavioral or maintenance benefit.
- Move fewer final tests: rejected because it would fragment the cohesive
  quality-gate group and leave the retained file closer to the ceiling.
- Refactor shared helpers or production code simultaneously: rejected because
  it expands scope beyond a safe mechanical relocation.

## Risks

- Security: low; production source, authentication, secrets, process
  execution, input handling, and persistence implementation are unchanged.
- Compatibility: low; all logical test paths and filters remain unchanged.
- Performance: none expected outside negligible test compilation layout.
- Test integrity: low if exact retained/moved comparisons and ordered inventory
  checks pass; all assertions and rejection cases remain mandatory.
- Maintenance: improved by separating quality-gate reducer tests from
  PR-feedback and stale-completion reducer tests.

## Test Plan

- [ ] Run the exact mechanical source checks below from the implementation
      worktree:

  ```bash
  BASE="$(git merge-base HEAD origin/main)"
  SOURCE=crates/harness-workflow/src/runtime/tests/completion_reducer_quality.rs
  RETAINED=$SOURCE
  MOVED=crates/harness-workflow/src/runtime/tests/completion_reducer_quality_gate.rs
  INCLUDES=crates/harness-workflow/src/runtime/tests.rs

  diff -u \
    <(git show "$BASE:$SOURCE" | sed -n '1,498p') \
    "$RETAINED"
  diff -u \
    <(git show "$BASE:$SOURCE" | sed -n '499,896p') \
    "$MOVED"
  diff -u \
    <(git show "$BASE:$INCLUDES" | awk '
      { print }
      $0 == "include!(\"tests/completion_reducer_quality.rs\");" {
        print "include!(\"tests/completion_reducer_quality_gate.rs\");"
      }') \
    "$INCLUDES"
  diff -u \
    <(git show "$BASE:$SOURCE" |
      sed -nE 's/^(async )?fn ([A-Za-z0-9_]+)\(.*/runtime::tests::\2/p') \
    <(sed -nE \
      's/^(async )?fn ([A-Za-z0-9_]+)\(.*/runtime::tests::\2/p' \
      "$RETAINED" "$MOVED")
  ```

- [ ] Baseline and final `cargo check -p harness-workflow --all-targets`.
- [ ] Baseline and final
      `cargo test -p harness-workflow completion_reducer`, with the same 47
      selected tests passing and unchanged paths.
- [ ] Final
      `cargo test -p harness-workflow quality_gate_run_decision_starts_runtime_activity`
      passes the moved test omitted by the `completion_reducer` filter.
- [ ] Final `cargo test -p harness-workflow --lib`.
- [ ] `cargo fmt --all -- --check`, line-count, exact retained file, exact
      moved file, ordered inventory, namespace, include adjacency, and
      three-file scope checks.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` and workspace
      tests under the repository test environment.
- [ ] SpecRail global/packet/route checks, manual VibeGuard L1-L7 review,
      exact-head CI, review-thread audit, and required PR gate.

## Rollback Plan

Squash-revert the implementation PR. No data, schema, migration,
configuration, dependency, or operator rollback step is required.
