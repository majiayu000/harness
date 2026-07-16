# Tech Spec

## Linked Issue

GH-1683

## Product Spec

See `specs/GH1683/product.md`.

## Current System

On base `02fba1d27c2272d2735c95ede29fcbf447aa8f3a`,
`crates/harness-workflow/src/runtime/tests/pr_repair_evidence.rs` contains 833
lines and 22 `#[test]` functions. Lines 1-448 contain 11 tests covering PR
hygiene decisions, structured ready-output rejection, closed-issue precedence,
and repair-snapshot evidence. Lines 449-833 contain 11 cohesive tests covering
server-owned PR readiness snapshots, completeness and review-thread rejection,
and child PR-feedback readiness evidence.

The root test module imports its parent with `use super::*`. A nested child
module can import the root test namespace the same way without changing
production visibility.

## Proposed Design

Keep exact-base lines 1-448 in `pr_repair_evidence.rs`, then append:

```rust
mod readiness_evidence;
```

Create
`crates/harness-workflow/src/runtime/tests/pr_repair_evidence/readiness_evidence.rs`
with `use super::*;`, one blank line, and exact-base lines 449-833 in their
current order.

The 11 retained tests keep their current logical paths. The 11 moved tests
intentionally become
`runtime::tests::pr_repair_evidence::readiness_evidence::<test_name>`. The
existing parent filter continues to select all 22 tests.

## Moved Test Inventory

1. `ready_to_merge_signal_with_current_pr_snapshot_starts_quality_gate`
2. `ready_to_merge_signal_with_missing_review_thread_completeness_blocks`
3. `ready_to_merge_signal_with_false_review_thread_completeness_blocks`
4. `ready_to_merge_signal_from_agent_sweep_with_server_snapshot_blocks`
5. `ready_to_merge_signal_with_only_agent_repair_snapshot_blocks`
6. `ready_to_merge_snapshot_accepts_quoted_pr_number`
7. `ready_to_merge_snapshot_for_different_pr_blocks`
8. `ready_to_merge_snapshot_with_required_review_blocks`
9. `ready_to_merge_snapshot_with_unresolved_thread_array_blocks_even_when_count_zero`
10. `child_pr_feedback_ready_signal_without_snapshot_blocks`
11. `child_pr_feedback_ready_snapshot_for_different_subject_pr_blocks`

## Invariants

- Record the exact base SHA, root line count, ordered 22-test inventory,
  attributes, and split boundary before editing.
- Require final root lines 1-448 to match the exact base byte-for-byte and the
  only appended root content to be `mod readiness_evidence;`.
- Generate the expected child file from `use super::*;`, a blank line, and
  exact-base lines 449-833; require an exact comparison after repository
  `rustfmt`.
- Preserve all 22 attributes, names, bodies, assertions, helper calls,
  artifacts, signals, JSON values, strings, and non-whitespace Rust tokens
  exactly once.
- Require the 11 retained paths to remain unchanged and the 11 moved paths to
  differ only by the new `readiness_evidence` segment.
- Require both formatted files to contain no more than 800 lines and no file
  outside the approved pair to change.

## Affected Files

- `crates/harness-workflow/src/runtime/tests/pr_repair_evidence.rs`
- `crates/harness-workflow/src/runtime/tests/pr_repair_evidence/readiness_evidence.rs`

No production source, other test, manifest, lockfile, configuration,
persistence, schema, or serialized-format file is expected to change.

## Data Flow

Workflow instances, activity results, artifacts, signals, reducer decisions,
quality-gate commands, review-thread evidence, and child workflows do not
change. Only Rust test module layout changes under the existing test-only
namespace.

## Alternatives Considered

- Leave the file unchanged: rejected because it remains above the hard ceiling
  and mixes a bounded final readiness-evidence group with repair evidence.
- Move only the final child-workflow tests: rejected because the root would
  remain close to the hard ceiling and the readiness snapshot group would stay
  fragmented.
- Move all readiness-related tests from non-contiguous regions: rejected
  because it makes the mechanical proof more complex without being necessary
  to satisfy the ceiling.
- Refactor helpers or production code simultaneously: rejected because it
  expands scope beyond a safe mechanical relocation.

## Risks

- Security: low; production source, authentication, secrets, process
  execution, input handling, and persistence implementation are unchanged.
- Compatibility: low; only 11 test paths gain one documented module segment,
  while the stable parent filter still selects the complete suite.
- Performance: none expected outside negligible test compilation layout.
- Test integrity: low if exact retained/moved comparisons and ordered inventory
  checks pass; assertions and rejection cases remain mandatory.
- Maintenance: improved by separating the final readiness-evidence group from
  the root repair-evidence tests.

## Test Plan

- [ ] Baseline and final `cargo check -p harness-workflow --all-targets`.
- [ ] Baseline and final
      `cargo test -p harness-workflow pr_repair_evidence`, with all 22 tests
      passing and documented paths.
- [ ] Final `cargo test -p harness-workflow --lib`.
- [ ] `cargo fmt --all -- --check`, line-count, exact root, exact child,
      ordered inventory, namespace, and two-file scope checks.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` and workspace
      tests under the repository test environment.
- [ ] SpecRail global/packet/route checks, manual VibeGuard L1-L7 review,
      exact-head CI, review-thread audit, and required PR gate.

## Rollback Plan

Squash-revert the implementation PR. No data, schema, migration,
configuration, dependency, or operator rollback step is required.
