# Product Spec

## Linked Issue

GH-1649

## User Problem

Harness maintainers must review and change PR-feedback workflow behavior in a
single 1,677-line production file that combines lifecycle entry points,
persistence, command selection, suppression, target loading, and runtime data
construction. The file is referenced by HTTP routes, background loops,
reconciliation, and task execution, so an apparently local edit has a broad
review surface and an elevated risk of accidental state-machine drift.

## Goals

- Give each PR-feedback workflow responsibility one clear production-module
  owner while retaining the existing root module as the stable caller facade.
- Reduce the root facade and every touched production module to no more than
  800 formatted lines.
- Preserve every existing caller path, workflow decision, persistence effect,
  command query, suppression rule, and user-visible outcome.
- Make future reviews able to isolate entry-point, persistence, command-state,
  and runtime-data changes without reading the full lifecycle implementation.
- Provide deterministic evidence that the extraction is complete and purely
  mechanical.

## Non-Goals

- Changing PR feedback, local review, merge approval, hygiene repair, or sweep
  policy.
- Changing workflow definitions, states, transitions, evidence, commands,
  dedupe keys, or persistence retry behavior.
- Changing JSON fields, defaulting, validation, log severity, errors, HTTP
  contracts, database schemas, migrations, runtime models, or dependencies.
- Renaming public or `pub(crate)` types/functions or moving tests for cosmetic
  reasons.
- Fixing unrelated behavior or performance findings discovered during the
  extraction.

## User-Visible Behavior

There is no intended user-visible behavior change. Requests that record PR
detection, feedback, local-review completion, PR merge, feedback sweeps,
hygiene repair, or merge approval must produce the same workflow state,
commands, evidence, task identifiers, persistence operations, events, logs,
errors, and return values as current `origin/main`.

## Acceptance Criteria

- [ ] B-001: All existing public and `pub(crate)` module paths, signatures,
      types, enum variants, and return values remain compatible.
- [ ] B-002: The formatted root facade and every touched production child
      module contain no more than 800 lines.
- [ ] B-003: A complete pre/post multiset of functions and types matches
      exactly, and every original symbol has exactly one implementation owner.
- [ ] B-004: PR detection, feedback, local-review, merge, sweep, hygiene, and
      merge-approval decisions retain their exact inputs and outcomes.
- [ ] B-005: Workflow persistence, retries, failure handling, event ordering,
      and decision-commit behavior remain unchanged.
- [ ] B-006: Active-command checks, deduplication, child-workflow selection,
      and failed-child suppression retain the same queries and cutoffs.
- [ ] B-007: Workflow target loading, instance identity, task IDs, definition
      IDs, and runtime JSON data remain byte-for-byte compatible in behavior.
- [ ] B-008: Existing unit, integration, database-backed server, workspace,
      Clippy, CI, review-thread, VibeGuard, and SpecRail gates pass.
- [ ] B-009: No manifest, lockfile, dependency, migration, schema, model, API,
      or test file is changed.
- [ ] B-010: The work is delivered as a separate implementation PR only after
      this spec PR is merged.

## Edge Cases

- Issue-backed and PR-scoped workflows must retain their different identity
  and target-loading paths.
- Missing optional PR URLs, repositories, issue numbers, and runtime JSON
  fields must keep their current defaults and validation failures.
- Concurrent or repeated sweep/local-review requests must retain current active
  command and failed-child suppression behavior.
- Persistence retries, rejected decisions, terminal workflows, and merge
  approval from non-candidate states must preserve current errors and outcomes.
- Test-only visibility used by the existing nested test modules must remain
  available without widening production APIs.

## Rollout Notes

This is a source-layout-only refactor. It requires no migration, feature flag,
configuration change, operator action, or compatibility communication. A
normal revert of the implementation PR is the rollback path.
