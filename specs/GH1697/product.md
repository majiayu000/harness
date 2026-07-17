# Product Spec

## Linked Issue

GH-1697

## User Problem

Harness maintainers must navigate a 900-line
`runtime/tests/runtime_store.rs` module that exceeds the repository's
800-line hard ceiling. Eleven behavior tests are interrupted by a contiguous
213-line block of nine database fixture and assertion helpers.

## Goals

- Extract the nine helpers to a direct support include.
- Reduce both resulting formatted files below 800 lines.
- Preserve every test, helper, assertion, source byte, and logical test path.
- Prove that the change is a mechanical test-source relocation with no
  runtime-store or production behavior change.

## Non-Goals

- Changing runtime-store behavior, SQL, migrations, persistence, or schemas.
- Adding, deleting, renaming, consolidating, reordering, or rewriting tests,
  helpers, or assertions.
- Changing production source, APIs, dependencies, manifests, or lockfiles.
- Splitting unrelated workflow test sources.

## User-Visible Behavior

There is no intended user-visible behavior change. Runtime-job status,
referential-integrity, dispatch, migration, listing, retention, and dedupe
behavior must remain identical to current `origin/main`.

## Acceptance Criteria

- [ ] B-001: `runtime_store.rs` is exactly 688 formatted lines and
      `runtime_store_support.rs` is exactly 213 formatted lines.
- [ ] B-002: The support file equals exact-base lines 620-832 byte-for-byte.
- [ ] B-003: The main file equals exact-base lines 1-619, followed by exactly
      `include!("runtime_store_support.rs");`, followed by exact-base lines
      833-900.
- [ ] B-004: All 11 test names, attributes, bodies, logical paths, nine helper
      names/bodies, and 43 assertion/expectation occurrences remain unchanged.
- [ ] B-005: Only the approved two test files change.
- [ ] B-006: No production behavior, source, API, dependency, manifest,
      lockfile, persistence, schema, or serialized-format change.
- [ ] B-007: Formatting, diff, source, inventory, compiled-path, focused/full
      test, package, workspace Clippy/test, VibeGuard, exact-head CI,
      review-thread, independent-review, and SpecRail gates pass.
- [ ] B-008: Implementation is delivered in a separate PR only after this
      specification PR merges.

## Edge Cases

- The support file must use `include!`, not a nested module, so all items remain
  in `runtime::tests::runtime_store`.
- Exact-base line 833 is the blank separator before the final dedupe test and
  remains in the main file.
- The support file must end at line 832's closing brace, not line 833's blank
  separator, so committed-whitespace checks pass.
- Item order in Rust does not affect helper availability; tests before and
  after the include must continue to call the same helpers.

## Rollout Notes

This test-source layout refactor requires no migration, feature flag,
configuration, or operator action. Squash-reverting the implementation PR is
the rollback path.
