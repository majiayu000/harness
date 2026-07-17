# Product Spec

## Linked Issue

GH-1665

## User Problem

Harness maintainers must navigate a 985-line `webhook.rs` file even though the
production implementation ends at line 333 and the remaining 652 lines are a
cohesive inline test module. The combined file exceeds the repository 800-line
hard ceiling and makes production webhook changes harder to review because
fixtures and assertions occupy roughly two thirds of the source file.

## Goals

- Give the existing GitHub webhook tests a dedicated private child module while
  retaining their current parent module and test names.
- Reduce both resulting Rust files to no more than 800 formatted lines.
- Preserve the production implementation exactly and preserve every test's
  Rust tokens, assertion, fixture, attribute, and coverage point; only
  deterministic whitespace reflow required by `rustfmt` is permitted.
- Make webhook production changes reviewable without traversing the complete
  test suite in the same physical source file.
- Provide deterministic evidence that the extraction is mechanical.

## Non-Goals

- Changing webhook event parsing, HMAC verification, issue/comment/review
  routing, label handling, request construction, error behavior, or logs.
- Adding, deleting, renaming, consolidating, or rewriting tests.
- Changing public APIs, private production visibility, configuration,
  dependencies, manifests, lockfiles, schemas, persisted formats, or prompts.
- Addressing unrelated webhook behavior or coverage findings.

## User-Visible Behavior

There is no intended user-visible behavior change. Supported GitHub issue,
comment, pull-request review, ping, signature, and invalid-payload inputs must
produce the same task requests, ignored-event reasons, errors, and validation
results as current `origin/main`.

## Acceptance Criteria

- [ ] B-001: `webhook.rs` and the extracted test module each contain no more
      than 800 formatted lines.
- [ ] B-002: The complete production function/type inventory and every
      production item body remain unchanged.
- [ ] B-003: All 33 test names, non-whitespace Rust tokens, assertions,
      fixtures, attributes, ordering, and `webhook::tests::*` module paths
      remain unchanged. Whitespace may differ only when the unindented baseline
      test body is canonically formatted by the repository `rustfmt` version.
- [ ] B-004: No production behavior, public API, visibility, dependency,
      manifest, lockfile, schema, persisted format, or test expectation changes.
- [ ] B-005: Focused webhook tests, `harness-server` checks/tests, workspace
      Clippy/tests, VibeGuard, exact-head CI, review-thread audit, and SpecRail
      gates pass.
- [ ] B-006: The implementation is delivered in a separate PR only after this
      spec PR is merged.

## Edge Cases

- Tests must retain child-module access to private webhook payload types and
  parsing helpers through the existing `use super::*` relationship without
  widening production visibility.
- HMAC fixtures, JSON payload strings, case-insensitive review states, empty
  bodies, invalid payloads, label filters, and ignored actions must move without
  token rewriting; indentation-driven `rustfmt` line wrapping is permitted.
- Test filtering by the existing `webhook::tests::...` path must continue to
  select the same 33 tests.
- The module declaration remains test-only and must not affect production
  builds or exported symbols.

## Rollout Notes

This is a test-source layout refactor only. It requires no migration, feature
flag, configuration change, operator action, or compatibility communication.
Squash-reverting the implementation PR is the rollback path.
