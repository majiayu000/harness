# Product Spec

## Linked Issue

GH-1690

## User Problem

Harness maintainers must navigate an 896-line
`runtime/tests/completion_reducer_quality.rs` file that exceeds the
repository's 800-line hard ceiling. Its first 13 tests cover PR feedback,
structured-decision rejection, stale completions, and terminal workflow
handling, while its final 10 tests form a contiguous quality-gate group.

## Goals

- Give the final 10 quality-gate reducer tests a dedicated sibling include
  file.
- Reduce both resulting formatted test files to no more than 800 lines.
- Preserve all 23 test names, attributes, bodies, assertions, fixtures,
  strings, and non-whitespace Rust tokens exactly.
- Preserve every logical test path by keeping both files included directly in
  the existing `runtime::tests` module.
- Provide deterministic evidence that the split is a mechanical test-source
  relocation with no workflow-runtime or production change.

## Non-Goals

- Changing PR-feedback, quality-gate, retry, reducer, validation, workflow, or
  terminal-state behavior.
- Adding, deleting, renaming, consolidating, or rewriting tests or helpers.
- Changing production source, public APIs, dependencies, manifests, lockfiles,
  persistence, schemas, or serialized formats.
- Splitting unrelated workflow-runtime test sources.

## User-Visible Behavior

There is no intended user-visible behavior change. PR feedback decisions,
quality-gate dispatch and completion, evidence rejection, retries, stale
completion handling, and terminal workflow handling must remain identical to
current `origin/main`.

## Acceptance Criteria

- [ ] B-001: `completion_reducer_quality.rs` and
      `completion_reducer_quality_gate.rs` each contain no more than 800
      formatted lines.
- [ ] B-002: Exact-base lines 1-498 remain byte-for-byte unchanged in
      `completion_reducer_quality.rs`.
- [ ] B-003: Exact-base lines 499-896 appear byte-for-byte unchanged and in
      order in `completion_reducer_quality_gate.rs`.
- [ ] B-004: `runtime/tests.rs` adds exactly one sibling
      `include!("tests/completion_reducer_quality_gate.rs");` immediately after
      the existing quality reducer include and changes nothing else.
- [ ] B-005: All 23 test names, attributes, bodies, assertions, fixtures,
      strings, and non-whitespace Rust tokens remain present exactly once, and
      all 23 logical test paths remain unchanged.
- [ ] B-006: No production behavior, production source, public API,
      dependency, manifest, lockfile, persistence, schema, or serialized
      format changes.
- [ ] B-007: Focused and full `harness-workflow` tests, workspace Clippy/tests,
      manual VibeGuard review, exact-head CI, review-thread audit, and SpecRail
      gates pass.
- [ ] B-008: The implementation is delivered in a separate PR only after this
      specification PR is merged.

## Edge Cases

- Both files must remain direct `include!` sources in `runtime::tests`; adding
  a Rust module would change test paths and violate B-005.
- The existing `completion_reducer` filter must continue selecting all current
  reducer tests, including both source files.
- Quality-gate rejection tests for missing, conflicting, invalid, and
  incomplete evidence must remain exact; test integrity must not be weakened.
- No test may be duplicated or omitted across the split boundary.
- The sibling include must remain adjacent so source organization mirrors the
  conceptual reducer grouping.

## Rollout Notes

This is a test-source layout refactor only. It requires no migration, feature
flag, configuration change, operator action, or compatibility communication.
Squash-reverting the implementation PR is the rollback path.
