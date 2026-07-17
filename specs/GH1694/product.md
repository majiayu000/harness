# Product Spec

## Linked Issue

GH-1694

## User Problem

Harness maintainers must navigate a 953-line
`runtime/tests/worker_lifecycle.rs` file that exceeds the repository's
800-line hard ceiling. Its first eight tests cover runtime-job claiming,
scheduling, leases, and baseline completion, while its final seven tests form
a contiguous issue-workflow completion-transition group.

## Goals

- Give the final seven completion-transition tests a dedicated sibling include
  file.
- Reduce both resulting formatted test files to no more than 800 lines.
- Preserve all 15 test names, attributes, bodies, assertions, fixtures,
  strings, and source bytes exactly.
- Preserve every logical test path by keeping both files included directly in
  the existing `runtime::tests` module.
- Provide deterministic evidence that the split is a mechanical test-source
  relocation with no workflow-runtime or production change.

## Non-Goals

- Changing runtime-job claiming, scheduling, lease, completion, PR-binding,
  implementation-evidence, child-propagation, or transaction behavior.
- Adding, deleting, renaming, consolidating, or rewriting tests or helpers.
- Changing production source, public APIs, dependencies, manifests, lockfiles,
  persistence, schemas, or serialized formats.
- Splitting unrelated workflow-runtime test sources.

## User-Visible Behavior

There is no intended user-visible behavior change. Runtime-job claiming,
scheduling, lease renewal, completion events, PR binding, implementation
evidence checks, child completion propagation, and transactional parent
completion must remain identical to current `origin/main`.

## Acceptance Criteria

- [ ] B-001: `worker_lifecycle.rs` and
      `worker_completion_transitions.rs` each contain no more than 800
      formatted lines.
- [ ] B-002: Exact-base lines 1-457 remain byte-for-byte unchanged in
      `worker_lifecycle.rs`.
- [ ] B-003: Exact-base lines 458-953 appear byte-for-byte unchanged and in
      order in `worker_completion_transitions.rs`.
- [ ] B-004: Concatenating the two final files reproduces the exact 953-line
      base source byte-for-byte.
- [ ] B-005: `runtime/tests.rs` adds exactly one sibling
      `include!("tests/worker_completion_transitions.rs");` immediately after
      the existing worker-lifecycle include and changes nothing else.
- [ ] B-006: All 15 test names, attributes, bodies, assertions, fixtures,
      strings, and source bytes remain present exactly once; all 15 logical
      test paths and 145 assertion/expectation sites remain unchanged.
- [ ] B-007: No production behavior, production source, public API,
      dependency, manifest, lockfile, persistence, schema, or serialized
      format changes.
- [ ] B-008: Focused and full `harness-workflow` tests, workspace Clippy/tests,
      manual VibeGuard review, exact-head CI, review-thread audit, and SpecRail
      gates pass.
- [ ] B-009: The implementation is delivered in a separate PR only after this
      specification PR is merged.

## Edge Cases

- Both files must remain direct `include!` sources in `runtime::tests`; adding
  a Rust module would change test paths and violate B-006.
- Existing `runtime_worker` and `runtime_store_` filters must continue
  selecting every current test with those names.
- PR-evidence rejection and transactional rollback tests must remain exact;
  test integrity must not be weakened.
- No test or helper source may be duplicated or omitted across the split
  boundary.
- Exact-base line 458 is the blank separator before the completion-transition
  group. It must move to the start of the sibling file so the retained file
  does not end with a blank line rejected by the committed-whitespace check.
- The sibling include must remain adjacent so source organization mirrors the
  conceptual worker grouping.

## Rollout Notes

This is a test-source layout refactor only. It requires no migration, feature
flag, configuration change, operator action, or compatibility communication.
Squash-reverting the implementation PR is the rollback path.
