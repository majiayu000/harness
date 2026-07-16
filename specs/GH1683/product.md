# Product Spec

## Linked Issue

GH-1683

## User Problem

Harness maintainers must navigate an 833-line
`runtime/tests/pr_repair_evidence.rs` file that exceeds the repository's
800-line hard ceiling. The final 11 tests form a cohesive readiness-evidence
group, while the preceding tests cover PR hygiene decisions, direct structured
ready-output rejection, closed-issue precedence, and repair-snapshot evidence.

## Goals

- Give the final 11 PR readiness-evidence tests a dedicated private child
  module.
- Reduce both resulting formatted test files to no more than 800 lines.
- Preserve all 22 test names, attributes, bodies, assertions, fixtures,
  strings, and non-whitespace Rust tokens exactly.
- Keep the 11 retained test paths unchanged and change the 11 moved paths only
  by adding the `readiness_evidence` module segment.
- Provide deterministic evidence that the split is a mechanical test-source
  relocation with no workflow-runtime or production change.

## Non-Goals

- Changing PR repair, PR readiness, quality-gate, review-thread, reducer,
  validation, or child-workflow behavior.
- Adding, deleting, renaming, consolidating, or rewriting tests or helpers.
- Changing production source, public APIs, dependencies, manifests, lockfiles,
  persistence, schemas, or serialized formats.
- Splitting unrelated workflow-runtime test modules.

## User-Visible Behavior

There is no intended user-visible behavior change. PR repair evidence,
server-owned readiness snapshots, quality-gate dispatch, review-thread
completeness, child PR-feedback workflows, and workflow decisions must remain
identical to current `origin/main`.

## Acceptance Criteria

- [ ] B-001: `pr_repair_evidence.rs` and
      `pr_repair_evidence/readiness_evidence.rs` each contain no more than 800
      formatted lines.
- [ ] B-002: Exact-base lines 1-448 remain byte-for-byte unchanged in the root
      file, apart from appending the one child-module declaration.
- [ ] B-003: Exact-base lines 449-833 appear byte-for-byte unchanged and in
      order after `use super::*;` in the child file.
- [ ] B-004: All 22 test names, attributes, bodies, assertions, fixtures,
      strings, and non-whitespace Rust tokens remain present exactly once. The
      11 retained paths stay unchanged; the 11 moved paths gain only
      `readiness_evidence`.
- [ ] B-005: No production behavior, production source, public API,
      dependency, manifest, lockfile, persistence, schema, or serialized
      format change.
- [ ] B-006: Focused and full `harness-workflow` tests, workspace Clippy/tests,
      manual VibeGuard review, exact-head CI, review-thread audit, and SpecRail
      gates pass.
- [ ] B-007: The implementation is delivered in a separate PR only after this
      specification PR is merged.

## Edge Cases

- The child file must retain access to the parent test module's imported
  helpers and fixtures through `use super::*;` without widening visibility.
- The existing `pr_repair_evidence` filter must continue selecting all 22
  tests, including the nested child module.
- The moved tests must preserve contradictory, incomplete, wrong-PR, and child
  PR-feedback readiness evidence exactly; test integrity must not be weakened.
- No test may be duplicated or omitted across the split boundary.

## Rollout Notes

This is a test-source layout refactor only. It requires no migration, feature
flag, configuration change, operator action, or compatibility communication.
Squash-reverting the implementation PR is the rollback path.
