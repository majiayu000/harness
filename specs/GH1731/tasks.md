# Task Plan

## Linked Issue

GH-1731

## Spec Packet

- Product: `specs/GH1731/product.md`
- Tech: `specs/GH1731/tech.md`

## Implementation Tasks

- [ ] `SP1731-T1` — Owner: implementation agent; Done when: the audited dependency and public inventory contract are wired; Verify: `cargo check -p harness-core --all-targets`.
- [ ] `SP1731-T2` — Owner: implementation agent; Done when: the closed typed discovery table emits valid ordered ASC-001 components; Verify: `cargo test -p harness-core stack::inventory_tests::inventory_discovers_every_stack_and_language_validation_selector`.
- [ ] `SP1731-T3` — Owner: implementation agent; Done when: capability-relative traversal enforces containment and every resource bound; Verify: `cargo test -p harness-core stack::inventory_tests::symlink_swaps_remain_root_confined_and_hash_the_opened_target`.
- [ ] `SP1731-T4` — Owner: implementation agent; Done when: exhaustive positive and negative inventory fixtures pass; Verify: `cargo test -p harness-core stack::inventory_tests`.
- [ ] `SP1731-T5` — Owner: verification owner; Done when: additive scope and all final gates pass on the exact head; Verify: `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1731`.

### SP1731-T1 — Establish the dependency and public inventory contract

- Owner: implementation agent
- Files: `Cargo.toml`, `Cargo.lock`, `crates/harness-core/Cargo.toml`,
  `crates/harness-core/src/stack/mod.rs`,
  `crates/harness-core/src/stack/inventory.rs`
- Dependencies: ASC-001 / GH-1730 merged
- Covers: B-001, B-002, B-010, B-012
- Done when:
  - A baseline `cargo audit` result is recorded before any manifest or lockfile
    edit.
  - The audited `cap-std` dependency is added without downgrading another
    dependency.
  - The public options, result, entry, entry-class, and typed error-category
    APIs match `tech.md`, keep invariant-bearing fields private, and expose
    read-only accessors.
  - Invalid limits fail before the repository root is opened.
  - Options expose exact non-zero regular-file, opened-directory,
    aggregate-encountered-entry, depth, entries-per-directory, per-file byte,
    and aggregate-byte limits with the defaults and integer types in `tech.md`.
  - The result contains ordered entries and makes no ambient canonical-root
    claim.
- Verify:
  - `cargo audit`
  - `cargo check -p harness-core --all-targets`
  - `cargo test -p harness-core stack::inventory_tests::invalid_limits_fail_before_root_open`

### SP1731-T2 — Implement the closed typed discovery table

- Owner: implementation agent
- Files: `crates/harness-core/src/stack/inventory.rs`
- Dependencies: SP1731-T1
- Covers: B-002, B-003, B-004, B-005, B-006, B-007, B-011
- Done when:
  - One immutable typed rule table represents every B-003 exact path,
    root-only suffix, recursive directory, and directory-presence predicate.
  - Harness-native roots have their specific component kinds and no recursive
    catch-all `.harness` rule exists.
  - Missing exact entries are omitted only after a non-following
    `symlink_metadata` lookup returns `NotFound`.
  - Every regular file and the root `spec` predicate construct valid ASC-001
    components through the merged public API.
  - Output order is the lexicographic order of lossless portable locators.
- Verify:
  - `cargo test -p harness-core stack::inventory_tests::inventory_discovers_every_stack_and_language_validation_selector`
  - `cargo test -p harness-core stack::inventory_tests::missing_allowlisted_entries_emit_no_placeholders`
  - `cargo test -p harness-core stack::inventory_tests::component_kind_comes_from_matching_rule`
  - `cargo test -p harness-core stack::inventory_tests::filesystem_enumeration_order_does_not_change_inventory`

### SP1731-T3 — Implement capability-relative bounded traversal

- Owner: implementation agent
- Files: `crates/harness-core/src/stack/inventory.rs`
- Dependencies: SP1731-T1, SP1731-T2
- Covers: B-001, B-004, B-008, B-009, B-010, B-011, B-012
- Done when:
  - The caller-supplied root is opened once and all descendant operations stay
    relative to that directory capability.
  - Broken symlinks, escaping targets, non-regular entries, unreadable data,
    non-UTF-8 locators, ancestor cycles, and entries that disappear after
    `read_dir` return their specified typed error kinds without leaking ambient
    target paths.
  - A valid in-root symlink swap may select either valid target, and the digest
    covers exactly the bytes from the opened regular-file handle without
    reporting `entry_raced`.
  - Depth uses the allowlisted directory as depth 0; directory enumeration
    reads at most the checked N+1 sentinel; every yielded item charges the
    aggregate encountered-entry budget; each opened directory including the
    repository root charges the directory budget; regular-file and byte
    budgets are charged before another read or descent.
  - Traversal performs no repository write, process launch, network operation,
    hook invocation, or MCP connection.
- Verify:
  - `cargo test -p harness-core stack::inventory_tests::inventory_stays_bound_to_the_opened_root_handle`
  - `cargo test -p harness-core stack::inventory_tests::symlink_swaps_remain_root_confined_and_hash_the_opened_target`
  - `cargo test -p harness-core stack::inventory_tests::unreadable_and_non_utf8_entries_fail_without_lossy_locators`
  - `cargo test -p harness-core stack::inventory_tests::reads_never_exceed_remaining_aggregate_or_per_file_budget`
  - `cargo test -p harness-core stack::inventory_tests::every_traversal_limit_has_an_exact_boundary_fixture`

### SP1731-T4 — Add exhaustive inventory contract tests

- Owner: implementation agent
- Files: `crates/harness-core/src/stack/inventory_tests.rs`
- Dependencies: SP1731-T1, SP1731-T2, SP1731-T3
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008,
  B-009, B-010, B-011, B-012
- Done when:
  - Table-driven fixtures cover every allowlisted surface, every ASC-001
    mapping, unrelated-file exclusion, exact root suffix matching, and the
    non-recursive `spec` predicate.
  - Negative fixtures cover missing versus broken symlinks, root replacement,
    escaping and in-root symlink swaps, cycles, special files, unreadable
    files, non-UTF-8 paths, post-`read_dir` disappearance, invalid limits, and
    every exact resource boundary including aggregate entries and opened
    directories.
  - Unix executable-bit changes alter entry evidence without changing the
    content digest; non-Unix reports executable state as unobserved.
  - A deterministic injected read failure covers platforms where permission
    changes cannot reliably produce an unreadable file.
- Verify:
  - `cargo test -p harness-core stack::inventory_tests`
  - `cargo test -p harness-core`

### SP1731-T5 — Verify additive scope and hand off the final slice

- Owner: verification owner
- Files: all six implementation paths in the planned-changes manifest
- Dependencies: SP1731-T1, SP1731-T2, SP1731-T3, SP1731-T4
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008,
  B-009, B-010, B-011, B-012
- Done when:
  - The implementation changes only the six authorized paths and does not add
    a CLI command, persistence migration, runtime consumer, or prompt-loader
    behavior.
  - The post-dependency `cargo audit` passes and no dependency is downgraded.
  - The implementation PR uses `Fixes #1731`; this spec PR uses only
    `Refs #1731` and leaves the issue open.
  - Exact-head CI, independent local review, review threads, and the SpecRail
    PR gate are all green before merge.
- Verify:
  - `git diff --name-only origin/main...HEAD`
  - `cargo audit`
  - `cargo fmt --all`
  - `cargo fmt --all -- --check`
  - `cargo check -p harness-core --all-targets`
  - `cargo test -p harness-core`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `python3 checks/check_workflow.py --repo .`
  - `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1731`

## Parallelization

The implementation is serial. SP1731-T1 through SP1731-T3 share
`inventory.rs`, SP1731-T4 verifies that exact contract, and SP1731-T5 owns
shared verification. A read-only reviewer lane may inspect the exact diff
after SP1731-T5; no two writable lanes may edit these paths concurrently.

## Verification

- [ ] Product invariant set is exactly B-001 through B-012.
- [ ] The task coverage union is exactly B-001 through B-012.
- [ ] The baseline security audit runs before dependency edits.
- [ ] The post-change security audit and every command under SP1731-T5 pass on
      the exact implementation head.

## Handoff Notes

- PR #1761 is the heavy spec-only slice. It must merge without closing GH-1731.
- The implementation stays library-only. ASC-026 owns public CLI exposure, so
  GH-1731 verification targets `harness-core`.
- `cap_std::fs::Dir::canonicalize` returns a capability-relative path; the
  inventory therefore exposes no ambient canonical root.
- In-root symlink swaps are non-atomic but root-confined. The opened file handle
  is the observation authority and its returned bytes determine the digest.
- The planned-change manifest is exhaustive. Any implementation path outside
  its six entries requires a spec revision before code changes.
