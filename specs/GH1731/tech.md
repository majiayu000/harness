# Tech Spec

## Linked Issue

GH-1731

## Product Spec

See `specs/GH1731/product.md`.

<!-- specrail-planned-changes
{"issue":1731,"complete":true,"paths":["crates/harness-core/src/stack/inventory.rs","crates/harness-core/src/stack/inventory/rules.rs","crates/harness-core/src/stack/inventory/review_tests.rs","crates/harness-core/src/stack/inventory_tests.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012"]}
-->

## Current System

PR #1810 merged the first GH-1731 implementation as commit `f55eea8b`. The
merged service already:

- exposes the repository-observed inventory contract from
  `crates/harness-core/src/stack/mod.rs`;
- opens one `cap-std` repository root and keeps descendant access relative to
  that capability;
- represents the closed static allowlist, configured policy sources, typed
  errors, traversal budgets, deterministic output, and ASC-001 component
  construction in `crates/harness-core/src/stack/inventory.rs`;
- covers the baseline product contract in
  `crates/harness-core/src/stack/inventory_tests.rs`; and
- includes the audited dependency and manifest wiring in `Cargo.toml`,
  `Cargo.lock`, and `crates/harness-core/Cargo.toml`.

Those six merged implementation paths are historical delivery, not planned
work for this amendment. The public API, dependency set, allowlist vocabulary,
and caller integration remain unchanged.

The merged production and test files are respectively 800 and 794 lines on
`origin/main`. They contain the rule model, configured-rule normalization,
traversal engine, public-contract fixtures, and review-driven white-box
fixtures in two monolithic files. Adding the required remediation in place
would exceed the repository's 800-line hard ceiling.

Six final current-head P2 comments were posted before #1810 merged and remained
unresolved when the merge completed. GH-1731 was subsequently reopened because
all six findings still apply to the merged code:

1. a flexible configured source can erase a later exact-file requirement for
   the same locator;
2. recursive traversal starts at depth 1 beneath every allowlist root instead
   of preserving repository-relative depth;
3. a recursive symlink raced to a dangling link before `open_dir` is
   misclassified as `entry_raced`;
4. a Windows drive-relative configured source is silently treated as an
   out-of-scope absolute source;
5. suffix and recursive candidates undergo fallible classification before
   their native directory listing is sorted; and
6. unreadable-file coverage is conditionally skipped when the test process can
   open a mode-`000` file.

## Root Cause

The original implementation encoded the broad product invariants, but three
internal boundaries remained underspecified:

- rule merging recorded only "already present" and did not define how target,
  selector, and required-presence constraints compose across static and
  configured bindings for the same policy locator;
- traversal helpers accepted a local depth and a path-free open-error
  classifier, losing repository-relative position and stable post-race path
  evidence; and
- deterministic tests asserted final output ordering without forcing multiple
  competing failure candidates through the actual bounded read/traversal
  paths.

The remediation closes those boundaries. It does not broaden discovery or add
a consumer.

## Remediation Design

### Private Module Split

Keep `stack::inventory` as the only exposed inventory module and split its
private implementation as follows:

- `inventory.rs` retains the public service entry point, scan state,
  capability-relative traversal, bounded reads, typed error classification,
  and component emission.
- `inventory/rules.rs` owns the private static rule table, selector vocabulary,
  minimal `harness.toml` shape, configured-source normalization, and rule
  constraint merge.
- `inventory_tests.rs` retains black-box product and public-contract fixtures.
- `inventory/review_tests.rs` owns deterministic white-box seams and the six
  remediation regressions below.

Neither new module is public. Moving existing code must preserve visibility and
behavior. `stack/mod.rs`, manifests, and lockfiles are deliberately outside the
follow-up path manifest because the public API and dependency graph do not
change.

### 1. Preserve the Strictest Derived Constraint

Normalize configured sources before merging them. For one normalized locator
and component kind, compose target shape, directory selector, and
required-presence as separate constraints:

- `RuleTarget::File` is stricter than any directory-capable target and wins
  regardless of whether it comes from a static rule, `exec_policy_paths`,
  `requirements_path`, or their field order;
- therefore an exact configured source tightens an equivalent static recursive
  or file-or-directory rule to `File`;
- when no exact-file binding exists, a static directory target retains its
  closed selector instead of being replaced by the configured `md`/`toml`
  selector; the static selector remains recorded but is inapplicable while an
  exact-file constraint is active;
- equivalent flexible targets merge without another traversal;
- the binding is required when any configured source declares it; and
- the same locator under distinct component kinds still emits one component
  per kind while reusing one opened file observation.

Consequently, a locator listed in both `rules.discovery_paths` and
`rules.exec_policy_paths` must reject a directory with
`non_regular_entry`. The same is true when an exact configured source overlaps
a static recursive rule, such as `rules.requirements_path = "rules"`.
Reversing configured field order must not change behavior. This preserves
B-006: configured file sources select the exact file, while static selectors
continue to govern bindings that remain directory-capable.

Acceptance tests:

- `derived_exact_file_constraint_wins_over_flexible_source`
- `exact_configured_source_tightens_static_recursive_rule`
- `derived_rule_merge_is_field_order_independent`

### 2. Measure Depth from the Repository Root

The opened repository root is depth 0. An allowlist or configured directory's
initial traversal depth is the count of its lossless normalized
repository-relative components, not a constant 1. Each recursive descendant
increments that depth exactly once.

Exact-case validation may reuse cached native listings and charge directory
budgets, but it must not reset the traversal depth. For example,
`.harness/rules` is physically depth 2 and fails with `limit_exceeded` when
`max_depth` is 1, before recursively processing children.

Acceptance tests:

- `nested_allowlist_root_preserves_repository_relative_depth`
- `configured_directory_preserves_repository_relative_depth`

### 3. Reclassify Recursive Symlink Open Races

When a recursive candidate resolves as a directory but capability-relative
`open_dir` later returns `NotFound`, first recheck the candidate with
non-following metadata through the same root capability. A still-present
symlink is not sufficient evidence that its target is broken. Resolve and
reopen that target capability-relatively once before classifying:

- if the current symlink resolves to and opens a valid in-root directory,
  continue traversal from that reopened handle as a valid replacement target;
  the retry belongs to the original directory-open attempt and does not charge
  a second directory or depth unit;
- if the symlink itself remains present and capability-relative target
  resolution returns `NotFound`, return `broken_symlink`;
- if the candidate vanished, became a non-symlink, resolves to a non-directory,
  or changes again before the single reopen completes, return `entry_raced`;
  and
- resolution or reopen failures that prove containment, permission, or metadata
  faults retain their existing typed category.

The error locator is the complete lossless repository-relative candidate when
representable, or its nearest lossless ancestor otherwise. No ambient target
path or raw OS error string is serialized. The retry is bounded to one
re-resolution/reopen so repeated replacement cannot loop or evade budgets.

Acceptance tests:

- `recursive_symlink_open_failure_accepts_valid_replacement`
- `recursive_symlink_open_failure_rechecks_broken_link`
- `recursive_directory_disappearance_remains_entry_raced`

### 4. Reject Windows Drive-Relative Sources

Configured policy paths must distinguish absolute paths from drive-relative
paths lexically and consistently on every host:

- truly absolute sources remain outside the repository-scoped inventory and
  emit no component, as required by B-002 and B-003;
- `C:policy.toml` and equivalent drive-prefixed paths without an absolute root
  fail with `configured_source_invalid`; and
- portable repository-relative paths continue to reject backslashes, parent
  traversal, NUL, and empty normalized locators.

This validation must not depend on the host running Windows so that CI can
exercise the contract deterministically.

Acceptance test:

- `drive_relative_configured_source_fails_typed`

### 5. Sort Before Fallible Candidate Classification

Each bounded native directory listing is collected and charged exactly once,
then sorted by a stable native-name ordering before candidate-specific
operations that can fail. Only after sorting may traversal:

- resolve symlink targets;
- convert selected names to lossless portable locators;
- reject selected non-UTF-8 names;
- enforce file-versus-directory target constraints; or
- open selected files.

Apply the same ordering boundary to recursive selectors and root suffix rules.
Final successful entries remain sorted by normalized portable locator. For an
unchanged repository containing multiple invalid selected entries, reversed
enumeration input must produce the same first typed error and safe locator.

Acceptance tests:

- `fallible_recursive_classification_is_order_independent`
- `fallible_suffix_classification_is_order_independent`

### 6. Exercise Read Failure Through the Bounded Read Path

Refactor only the private bounded-read boundary needed for deterministic test
injection. The production path still reads from the already-opened regular-file
handle, honors the per-file and aggregate `+ 1` sentinels, and maps an actual
reader failure to `read_failed`.

The test seam must fail during `read_to_end` (or its equivalent bounded read),
not by directly asserting an error-classifier mapping. The fixture must always
execute, including as root or another privileged user, and must assert the safe
selected locator. Permission-based coverage may remain as a platform-specific
supplement, but it cannot conditionally skip the only end-to-end assertion.

Acceptance test:

- `injected_reader_failure_exercises_bounded_read_path`

## Compatibility and Scope

- The remediation preserves B-001 through B-012 and changes no public type,
  function signature, default limit, allowlist row, component kind, serialized
  value, or dependency.
- Absolute configured sources remain excluded; only the incorrectly omitted
  drive-relative form becomes a typed failure.
- The scan remains read-only and library-only. It performs no write,
  subprocess, network, hook, MCP, package-resolution, persistence, prompt, or
  workflow-runtime operation.
- Existing valid inventories retain their ordered entries and digests. The
  only behavior changes are fail-closed classification, constraint
  preservation, root-relative limit enforcement, and deterministic error
  selection for invalid or raced repositories.

## Planned Paths

The follow-up remediation PR is authorized to change exactly:

1. `crates/harness-core/src/stack/inventory.rs`
2. `crates/harness-core/src/stack/inventory/rules.rs`
3. `crates/harness-core/src/stack/inventory_tests.rs`
4. `crates/harness-core/src/stack/inventory/review_tests.rs`

Any public API, dependency, caller, product-spec, or additional implementation
path requires another spec amendment before code changes.

## Verification

The follow-up implementation must run:

- each named acceptance test in this spec;
- `cargo test -p harness-core stack::inventory_tests`;
- `cargo test -p harness-core`;
- `cargo check -p harness-core --all-targets`;
- `cargo fmt --all` and `cargo fmt --all -- --check`;
- `cargo clippy --workspace --all-targets -- -D warnings`;
- `python3 checks/check_workflow.py --repo .`;
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1731`; and
- `git diff --name-only <base>...HEAD`, proving the four-path manifest.

The implementation PR must reference the reopened GH-1731 and may close it only
after exact-head CI, independent review, and all review threads are clean.
