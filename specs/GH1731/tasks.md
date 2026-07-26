# Task Plan

## Linked Issue

GH-1731

## Spec Packet

- Product: `specs/GH1731/product.md`
- Tech: `specs/GH1731/tech.md`

## Delivery Context

PR #1810 merged the initial inventory implementation as `f55eea8b`. Six final
current-head P2 comments had been posted before that merge and remained
unresolved when it completed; GH-1731 was subsequently reopened. Tasks
SP1731-T1 through SP1731-T5 belong to the historical delivery and are not
reopened or repeated here. This plan continues with stable IDs SP1731-T6
through SP1731-T10 for their post-merge remediation.

## Implementation Tasks

- [ ] `SP1731-T6` — Owner: implementation agent; Done when: the merged rule model and review fixtures are moved into private `rules.rs` and `review_tests.rs` modules without public API, dependency, or behavior changes, and all existing inventory tests pass; Verify: `cargo test -p harness-core stack::inventory_tests`.
- [ ] `SP1731-T7` — Owner: implementation agent; Done when: configured-rule merging preserves the strictest exact-file constraint across configured and static overlaps independently of field order, preserves static selectors for bindings that remain directory-capable, and rejects drive-relative Windows sources typed, with deterministic regressions; Verify: `cargo test -p harness-core stack::inventory::review_tests::exact_configured_source_tightens_static_recursive_rule` and `cargo test -p harness-core stack::inventory::review_tests::drive_relative_configured_source_fails_typed`.
- [ ] `SP1731-T8` — Owner: implementation agent; Done when: recursive depth is measured from repository depth 0 and recursive symlink open failures perform one capability-relative target re-resolution/reopen to distinguish a valid replacement, broken target, and raced entry, with deterministic regressions; Verify: `cargo test -p harness-core stack::inventory::review_tests::nested_allowlist_root_preserves_repository_relative_depth` and `cargo test -p harness-core stack::inventory::review_tests::recursive_symlink_open_failure_accepts_valid_replacement`.
- [ ] `SP1731-T9` — Owner: implementation agent; Done when: bounded native listings are sorted before fallible recursive/suffix classification and an injected reader failure always traverses the actual bounded-read path, with order-reversal and privileged-user-safe regressions; Verify: `cargo test -p harness-core stack::inventory::review_tests::fallible_recursive_classification_is_order_independent` and `cargo test -p harness-core stack::inventory::review_tests::injected_reader_failure_exercises_bounded_read_path`.
- [ ] `SP1731-T10` — Owner: verification owner; Done when: every six-finding acceptance test, the full `harness-core` suite, formatting, check, workspace clippy, SpecRail checks, four-path scope audit, exact-head CI, and independent review gates pass; Verify: the commands under SP1731-T10 below.

### SP1731-T6 — Split Private Rules and Review Fixtures

- Owner: implementation agent
- Dependencies: merged PR #1810 (`f55eea8b`)
- Files:
  - `crates/harness-core/src/stack/inventory.rs`
  - `crates/harness-core/src/stack/inventory/rules.rs`
  - `crates/harness-core/src/stack/inventory_tests.rs`
  - `crates/harness-core/src/stack/inventory/review_tests.rs`
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-008, B-009,
  B-010, B-012
- Done when:
  - `inventory/rules.rs` privately owns `StaticRule`, matcher and target types,
    selector constants, `STATIC_RULES`, minimal config shapes,
    configured-source normalization, and rule merge helpers.
  - `inventory/review_tests.rs` privately owns white-box fixtures and
    deterministic seams; black-box public-contract fixtures remain in
    `inventory_tests.rs`.
  - `stack::inventory` remains the only exposed inventory module.
  - No public type, signature, default, serialized value, dependency,
    allowlist row, or component-kind mapping changes.
  - Production and test files remain below the repository 800-line ceiling.
  - The move is behavior-neutral and existing inventory tests pass before the
    six fixes are layered on top.
- Verify:
  - `cargo check -p harness-core --all-targets`
  - `cargo test -p harness-core stack::inventory_tests`
  - `git diff --check`

### SP1731-T7 — Preserve Rule Constraints and Validate Sources

- Owner: implementation agent
- Dependencies: SP1731-T6
- Files:
  - `crates/harness-core/src/stack/inventory/rules.rs`
  - `crates/harness-core/src/stack/inventory/review_tests.rs`
- Covers: B-002, B-003, B-004, B-006, B-009, B-011
- Done when:
  - A derived exact-file target wins over a flexible file-or-directory target
    for the same normalized locator and component kind, independently of
    configured field order.
  - An exact configured source also tightens an equivalent static recursive or
    file-or-directory rule to an exact-file target.
  - When no exact-file constraint exists, a static directory target preserves
    its closed selector; the selector remains recorded but does not apply while
    an exact-file target is active.
  - Any configured overlap makes the merged binding required.
  - Equivalent flexible bindings traverse once, and one locator with distinct
    component kinds retains separate entries backed by one file observation.
  - `C:policy.toml` and equivalent drive-relative forms fail with
    `configured_source_invalid` on every host.
  - Truly absolute configured sources remain out of repository scope, and
    existing invalid portable relative forms still fail typed.
- Verify:
  - `cargo test -p harness-core stack::inventory::review_tests::derived_exact_file_constraint_wins_over_flexible_source`
  - `cargo test -p harness-core stack::inventory::review_tests::exact_configured_source_tightens_static_recursive_rule`
  - `cargo test -p harness-core stack::inventory::review_tests::derived_rule_merge_is_field_order_independent`
  - `cargo test -p harness-core stack::inventory::review_tests::drive_relative_configured_source_fails_typed`
  - `cargo test -p harness-core stack::inventory_tests::configured_repository_rule_sources_are_inventoried_once`
  - `cargo test -p harness-core stack::inventory_tests::same_locator_with_distinct_kinds_is_preserved`

### SP1731-T8 — Preserve Root-Relative Depth and Race Evidence

- Owner: implementation agent
- Dependencies: SP1731-T6
- Files:
  - `crates/harness-core/src/stack/inventory.rs`
  - `crates/harness-core/src/stack/inventory/review_tests.rs`
- Covers: B-001, B-008, B-009, B-010, B-011
- Done when:
  - The repository root remains depth 0 and an initial allowlist or configured
    directory starts traversal at its normalized root-relative component count.
  - Exact-case listing reuse cannot reset or bypass depth enforcement.
  - A nested allowlist root fails before descent when its physical depth
    already exceeds `max_depth`.
  - Recursive `open_dir` `NotFound` rechecks non-following metadata and then,
    for a still-present symlink, performs one capability-relative target
    re-resolution/reopen through the same root.
  - A current valid in-root directory replacement continues traversal without
    a second directory/depth charge; a still-present symlink whose target
    resolution returns `NotFound` returns `broken_symlink`.
  - A vanished, non-symlink, non-directory, or repeatedly replaced candidate
    returns `entry_raced`; other containment, permission, and metadata failures
    retain their typed categories.
  - The retry is bounded to one attempt and cannot loop or evade limits.
  - Safe locators remain complete when losslessly representable and otherwise
    stop at the nearest lossless ancestor.
- Verify:
  - `cargo test -p harness-core stack::inventory::review_tests::nested_allowlist_root_preserves_repository_relative_depth`
  - `cargo test -p harness-core stack::inventory::review_tests::configured_directory_preserves_repository_relative_depth`
  - `cargo test -p harness-core stack::inventory::review_tests::recursive_symlink_open_failure_accepts_valid_replacement`
  - `cargo test -p harness-core stack::inventory::review_tests::recursive_symlink_open_failure_rechecks_broken_link`
  - `cargo test -p harness-core stack::inventory::review_tests::recursive_directory_disappearance_remains_entry_raced`
  - `cargo test -p harness-core stack::inventory_tests::every_traversal_limit_has_an_exact_boundary_fixture`

### SP1731-T9 — Make Failure Ordering and Read Coverage Deterministic

- Owner: implementation agent
- Dependencies: SP1731-T6, SP1731-T8
- Files:
  - `crates/harness-core/src/stack/inventory.rs`
  - `crates/harness-core/src/stack/inventory/review_tests.rs`
  - `crates/harness-core/src/stack/inventory_tests.rs`
- Covers: B-005, B-007, B-009, B-010, B-011, B-012
- Done when:
  - Each bounded native listing is collected and charged once, then sorted
    before symlink resolution, lossless locator conversion, target-class
    checks, or selected-file opens.
  - Recursive and root-suffix paths use the same sort-before-fallible-work
    boundary.
  - Reversing injected enumeration order produces the same first typed error
    and safe locator for multiple invalid selected candidates.
  - A private test seam injects failure during the bounded read of an
    already-opened selected regular file.
  - The injected failure always asserts `read_failed` and the selected locator;
    no privilege or permission probe can skip the only end-to-end assertion.
  - Production bounded reads retain the per-file and aggregate `+ 1` sentinel
    behavior.
- Verify:
  - `cargo test -p harness-core stack::inventory::review_tests::fallible_recursive_classification_is_order_independent`
  - `cargo test -p harness-core stack::inventory::review_tests::fallible_suffix_classification_is_order_independent`
  - `cargo test -p harness-core stack::inventory::review_tests::injected_reader_failure_exercises_bounded_read_path`
  - `cargo test -p harness-core stack::inventory_tests::unreadable_and_non_utf8_entries_fail_without_lossy_locators`
  - `cargo test -p harness-core stack::inventory_tests::filesystem_enumeration_order_does_not_change_inventory`

### SP1731-T10 — Verify and Hand Off the Remediation

- Owner: verification owner
- Dependencies: SP1731-T7, SP1731-T8, SP1731-T9
- Files: exactly the four paths in the `specrail-planned-changes` manifest
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008,
  B-009, B-010, B-011, B-012
- Done when:
  - All six final #1810 findings map to changed production logic and at least
    one deterministic acceptance test named in `tech.md`.
  - The implementation diff contains exactly the four authorized paths.
  - The public API, manifests, lockfile, allowlist semantics, and callers are
    unchanged.
  - Focused tests, the complete `harness-core` suite, formatting, check,
    warning-sensitive workspace clippy, and both SpecRail checks pass on the
    exact implementation head.
  - Exact-head CI is green and an independent reviewer confirms no unresolved
    actionable review threads before the follow-up PR closes GH-1731.
- Verify:
  - `cargo test -p harness-core stack::inventory::review_tests`
  - `cargo test -p harness-core stack::inventory_tests`
  - `cargo test -p harness-core`
  - `cargo check -p harness-core --all-targets`
  - `cargo fmt --all`
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `python3 checks/check_workflow.py --repo .`
  - `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1731`
  - `git diff --name-only <base>...HEAD`

## Dependency and Parallelization

The implementation is serial:

1. SP1731-T6 establishes the private module boundary.
2. SP1731-T7 and SP1731-T8 may be implemented only after that split, but both
   share `review_tests.rs`, so they remain one writable lane.
3. SP1731-T9 depends on the traversal seam from SP1731-T8.
4. SP1731-T10 is the sole full-verification owner.

A separate read-only reviewer may inspect the final exact head. No two writable
lanes may edit the four planned paths concurrently.

## Completion Checklist

- [ ] Product invariant set remains exactly B-001 through B-012.
- [ ] The union of task `Covers:` fields is exactly B-001 through B-012.
- [ ] Every final current-head #1810 finding has one named deterministic
      acceptance test.
- [ ] The planned-change manifest contains exactly four implementation paths.
- [ ] No public API, dependency, caller, or product-spec path changes.
- [ ] The follow-up implementation PR references the reopened GH-1731 and
      closes it only after exact-head CI and review gates pass.

## Handoff Notes

- PR #1810 is merged historical implementation, not pending work.
- PR #1812 is a post-merge remediation amendment and uses `Refs #1731`; it does
  not itself close the issue.
- The follow-up implementation should use `Fixes #1731` only after completing
  SP1731-T6 through SP1731-T10.
- No merge or review-thread resolution is authorized by this spec-only task.
