# Product Spec

## Linked Issue

GH-1659

## User Problem

Harness maintainers must navigate a 1,047-line `gc_agent.rs` file even though
the production implementation ends at line 626 and the remaining 421 lines
are a cohesive inline test module. The combined file exceeds the repository
800-line hard ceiling and makes production changes harder to review because
test fixtures and assertions dominate the lower half of the source file.

## Goals

- Give the existing GC-agent tests a dedicated private module while retaining
  their current parent module and test names.
- Reduce both resulting Rust files to no more than 800 formatted lines.
- Preserve the production implementation and all test bodies, assertions,
  fixtures, and coverage exactly.
- Make GC-agent production changes reviewable without traversing the complete
  test suite in the same source file.
- Provide deterministic inventory evidence that the extraction is mechanical.

## Non-Goals

- Changing GC signal detection, draft generation, adoption, rejection,
  checkpoint, artifact parsing, path validation, or auto-adoption behavior.
- Adding, deleting, renaming, consolidating, or rewriting tests.
- Changing public APIs, private production visibility, errors, logs, prompts,
  configuration, dependencies, manifests, lockfiles, or persisted formats.
- Addressing unrelated GC behavior or coverage findings.

## User-Visible Behavior

There is no intended user-visible behavior change. GC runs, draft persistence,
artifact parsing, adoption and rejection decisions, target-path validation,
and auto-adoption must produce the same outputs, errors, logs, and side effects
as current `origin/main`.

## Acceptance Criteria

- [ ] B-001: `gc_agent.rs` and the extracted test module each contain no more
      than 800 formatted lines.
- [ ] B-002: The complete production function/type inventory and every
      production item body remain unchanged.
- [ ] B-003: All 15 test names, bodies, assertions, fixtures, and the
      `gc_agent::tests::*` module path remain unchanged.
- [ ] B-004: No production behavior, public API, visibility, dependency,
      manifest, lockfile, schema, persisted format, or test expectation changes.
- [ ] B-005: Focused and full `harness-gc` tests, workspace Clippy/tests,
      VibeGuard, exact-head CI, review-thread audit, and SpecRail gates pass.
- [ ] B-006: The implementation is delivered in a separate PR only after this
      spec PR is merged.

## Edge Cases

- Tests must retain child-module access to private GC-agent helpers through the
  existing `use super::*` relationship without widening production visibility.
- Async and synchronous tests must retain their current attributes and names.
- Multi-line fixture strings, unified diffs, path traversal cases, and
  temporary-directory lifetimes must move without textual rewriting.
- Test filtering by the existing `gc_agent::tests::...` path must continue to
  select the same 15 tests.

## Rollout Notes

This is a test-source layout refactor only. It requires no migration, feature
flag, configuration change, operator action, or compatibility communication.
Squash-reverting the implementation PR is the rollback path.
