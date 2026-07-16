# Product Spec

## Linked Issue

GH-1680

## User Problem

Harness maintainers must navigate an 822-line
`http/tests/runtime_worker_tests.rs` file that exceeds the repository 800-line
hard ceiling. The file combines three runtime-job execution and terminal-state
tests with four cohesive tests for starting child workflows without an agent
turn.

## Goals

- Give the four child-workflow-start tests a dedicated private child module.
- Reduce both resulting test files to no more than 800 formatted lines.
- Preserve all seven test names, attributes, bodies, assertions, fixtures,
  database gates, and non-whitespace Rust tokens exactly.
- Keep the three retained test paths unchanged and change the four moved paths
  only by adding the `child_workflows` module segment.
- Provide deterministic evidence that the split is a mechanical test-source
  relocation with no production change.

## Non-Goals

- Changing runtime worker, child-workflow, runtime-turn, workflow-state,
  persistence, or database behavior.
- Adding, deleting, renaming, consolidating, or rewriting tests or fixtures.
- Changing production source, public APIs, dependencies, manifests, lockfiles,
  schemas, persistence, or serialized formats.
- Splitting unrelated HTTP test modules.

## User-Visible Behavior

There is no intended user-visible behavior change. Runtime jobs, child
workflows, agent-turn suppression, workflow state, persistence, and HTTP
behavior must remain identical to current `origin/main`.

## Acceptance Criteria

- [ ] B-001: `runtime_worker_tests.rs` and
      `runtime_worker_tests/child_workflows.rs` each contain no more than 800
      formatted lines.
- [ ] B-002: Exact-base lines 1-409 remain byte-for-byte unchanged in the root
      file, apart from appending the one child-module declaration.
- [ ] B-003: Exact-base lines 410-822 appear byte-for-byte unchanged and in
      order after `use super::*;` in the child file.
- [ ] B-004: All seven test names, attributes, bodies, assertions, fixtures,
      database gates, and non-whitespace Rust tokens remain present exactly
      once. The three retained paths stay unchanged; the four moved paths gain
      only `child_workflows`.
- [ ] B-005: No production behavior, production source, public API,
      dependency, manifest, lockfile, persistence, schema, or serialized
      format change.
- [ ] B-006: Focused PostgreSQL tests, `harness-server` checks/tests, workspace
      Clippy/tests, VibeGuard review, exact-head CI, review-thread audit, and
      SpecRail gates pass.
- [ ] B-007: The implementation is delivered in a separate PR only after this
      specification PR is merged.

## Edge Cases

- The child file must retain access to the parent test module's imported
  fixtures and helpers through `use super::*;` without widening visibility.
- The existing filter `http::tests::runtime_worker_tests` must continue to
  select all seven tests, including the nested child module.
- All seven tests must continue using the same dedicated PostgreSQL setup and
  `db_tests_enabled` gate.
- Child-workflow tests must retain their existing serialized execution order
  under `--test-threads=1` verification.

## Rollout Notes

This is a test-source layout refactor only. It requires no data migration,
feature flag, configuration change, operator action, or compatibility
communication. Squash-reverting the implementation PR is the rollback path.
