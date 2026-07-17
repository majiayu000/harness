# Product Spec

## Linked Issue

GH-1677

## User Problem

Harness maintainers must navigate an 803-line `config/intake.rs` file even
though one cohesive inline test module occupies lines 492-792. The combined
file exceeds the repository 800-line hard ceiling and mixes intake
configuration types, defaults, validation, lookup, and redacted-debug behavior
with 12 focused unit tests.

## Goals

- Give the existing intake configuration tests a dedicated private child
  module while retaining their current parent module and test names.
- Reduce both resulting Rust files to no more than 800 formatted lines.
- Preserve the production implementation exactly and preserve every test's
  non-whitespace Rust tokens, assertion, fixture, string, attribute, helper,
  and coverage point; only deterministic whitespace reflow required by
  `rustfmt` is permitted.
- Make intake configuration changes reviewable without traversing the complete
  test suite in the same physical source file.
- Provide deterministic evidence that the extraction is mechanical.

## Non-Goals

- Changing intake defaults, parsing, validation, repository lookup, merge or
  recovery policy, Feishu redaction, serialization, or debug behavior.
- Adding, deleting, renaming, consolidating, or rewriting tests or fixtures.
- Changing public APIs, private production visibility, dependencies,
  manifests, lockfiles, persistence, schemas, or serialized formats.
- Implementing the workflow-configuration work proposed by GH-1656 or
  addressing unrelated configuration findings.

## User-Visible Behavior

There is no intended user-visible behavior change. GitHub and Feishu intake
configuration defaults, parsing, validation, repository selection, policy
resolution, serialization, and redacted debug output must remain identical to
the exact `origin/main` baseline.

## Acceptance Criteria

- [ ] B-001: `config/intake.rs` and the extracted test module each contain no
      more than 800 formatted lines.
- [ ] B-002: Exact-base production lines 1-491 and 793-803, including the
      complete `IntakeConfig` definition after the test block, remain
      byte-for-byte unchanged.
- [ ] B-003: All 12 test names, attributes, order, assertions, fixtures,
      strings, calls, comments, non-whitespace Rust tokens, and
      `config::intake::tests::*` paths remain unchanged. Whitespace may differ
      only when the unindented baseline test body is canonically formatted by
      the repository `rustfmt` version.
- [ ] B-004: No intake behavior, public API, production visibility,
      dependency, manifest, lockfile, persistence, schema, or serialized
      format changes.
- [ ] B-005: Focused intake tests, `harness-core` checks/tests, workspace
      Clippy/tests, VibeGuard, exact-head CI, review-thread audit, and SpecRail
      gates pass.
- [ ] B-006: The implementation is delivered in a separate PR only after this
      specification PR is merged.

## Edge Cases

- Tests must retain child-module access to private intake constants and helper
  behavior through the existing `use super::*` relationship without widening
  production visibility.
- The `IntakeConfig` production type that follows the inline test block must
  remain present, ordered, and byte-for-byte unchanged.
- Test filtering by the existing `config::intake::tests::...` path must
  continue to select the same 12 tests.
- The module declaration remains test-only and must not affect production
  builds or exported symbols.

## Rollout Notes

This is a test-source layout refactor only. It requires no data migration,
feature flag, configuration change, operator action, or compatibility
communication. Squash-reverting the implementation PR is the rollback path.
