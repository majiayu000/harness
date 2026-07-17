# Product Spec

## Linked Issue

GH-1673

## User Problem

Harness maintainers must navigate an 802-line `runtime/worker.rs` file even
though the production workflow-runtime worker implementation ends at line 522
and the remaining 279 lines are one cohesive inline test module. The combined
file exceeds the repository 800-line hard ceiling and mixes runtime dispatch,
lease renewal, child completion, budget, and failure-side-effect logic with
five focused tests and their fixtures.

## Goals

- Give the existing runtime worker tests a dedicated private child module
  while retaining their current parent module and test names.
- Reduce both resulting Rust files to no more than 800 formatted lines.
- Preserve the production implementation exactly and preserve every test's
  non-whitespace Rust tokens, assertion, fixture, string, attribute, helper,
  and coverage point; only deterministic whitespace reflow required by
  `rustfmt` is permitted.
- Make workflow-runtime worker changes reviewable without traversing the
  complete worker test suite in the same physical source file.
- Provide deterministic evidence that the extraction is mechanical.

## Non-Goals

- Changing runtime dispatch, job claims, lease renewal, child completion,
  activity results, budgets, failure metadata, events, commands, or profiles.
- Adding, deleting, renaming, consolidating, or rewriting tests or fixtures.
- Changing public APIs, private production visibility, dependencies,
  manifests, lockfiles, persistence, database setup, or serialized formats.
- Addressing unrelated workflow-runtime behavior or coverage findings.

## User-Visible Behavior

There is no intended user-visible behavior change. Runtime worker job
selection, execution, lease management, child completion propagation, budget
enforcement, failure metadata, and database-backed tests must remain identical
to current `origin/main`.

## Acceptance Criteria

- [ ] B-001: `runtime/worker.rs` and the extracted test module each contain no
      more than 800 formatted lines.
- [ ] B-002: Lines 1-522 of the exact-base production implementation and the
      complete production item inventory remain byte-for-byte unchanged.
- [ ] B-003: All five test names, attributes, order, assertions, fixtures,
      strings, calls, helpers, non-whitespace Rust tokens, and
      `runtime::worker::tests::*` paths remain unchanged. Whitespace may differ
      only when the unindented baseline test body is canonically formatted by
      the repository `rustfmt` version.
- [ ] B-004: No workflow runtime behavior, public API, production visibility,
      dependency, manifest, lockfile, persistence, event, command, lease,
      dispatch, profile, or failure-side-effect change.
- [ ] B-005: Focused worker tests, `harness-workflow` checks/tests, workspace
      Clippy/tests, VibeGuard, exact-head CI, review-thread audit, and SpecRail
      gates pass.
- [ ] B-006: The implementation is delivered in a separate PR only after this
      specification PR is merged.

## Edge Cases

- Tests must retain child-module access to private worker helpers, traits, and
  failure-side-effect logic through the existing `use super::*` relationship
  without widening production visibility.
- Database-backed tests must continue to use the repository database resolver
  and retain their existing skip/error behavior, SQL-backed store calls, and
  serial execution coverage.
- Test filtering by the existing `runtime::worker::tests::...` path must
  continue to select the same five tests.
- The module declaration remains test-only and must not affect production
  builds or exported symbols.

## Rollout Notes

This is a test-source layout refactor only. It requires no data migration,
feature flag, configuration change, operator action, or compatibility
communication. Squash-reverting the implementation PR is the rollback path.
