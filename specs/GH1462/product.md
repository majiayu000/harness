# Product Spec

## Linked Issue

GH-1462

## User Problem

`crates/harness-workflow/src/runtime/tests.rs` remains a very large test file
even after earlier partial splits. On the current main branch it is 5,554 lines
and still carries the 6,500-line U-16 exemption in `CLAUDE.md`. The file
consumes too much agent context for routine workflow-runtime edits and remains a
merge-conflict hotspot.

Maintainers need this file split into focused test modules without changing the
test inventory or behavior.

## Goals

- Split the remaining giant `runtime/tests.rs` body into focused modules under
  `crates/harness-workflow/src/runtime/tests/`.
- Keep the change mechanical: preserve test names, assertions, fixture data, and
  behavior.
- Move shared fixtures, helper constructors, and test executors into a common
  test helper module consumed by the split files.
- Keep every resulting test file below an agreed ceiling of about 1,200 lines.
- Update the U-16 exemption in `CLAUDE.md` so `runtime/tests.rs` no longer has a
  6,500-line allowance.
- Prove the sorted `cargo test -p harness-workflow -- --list` inventory is
  unchanged before and after the split.

## Non-Goals

- Adding, removing, renaming, weakening, or improving tests.
- Changing workflow runtime behavior, schemas, reducers, validators, or command
  dispatch semantics.
- Splitting other oversized files.
- Reorganizing production modules or changing public APIs.

## User-Visible Behavior

There is no product runtime behavior change. Developers and agents working on
the workflow runtime see smaller, domain-focused test files instead of one
oversized root test file. Test execution and test names remain the same.

## Acceptance Criteria

- [ ] `cargo test -p harness-workflow -- --list` produces the same sorted test
      names before and after the split.
- [ ] The root `crates/harness-workflow/src/runtime/tests.rs` contains shared
      module declarations or is replaced by a module directory entrypoint, not a
      multi-thousand-line test body.
- [ ] Shared helpers are moved once and imported by focused modules; duplicate
      fixture copies are not introduced.
- [ ] No resulting test file in `crates/harness-workflow/src/runtime/tests/`
      exceeds about 1,200 lines.
- [ ] `CLAUDE.md` no longer grants
      `**/harness-workflow/src/runtime/tests.rs` a 6,500-line exemption.
- [ ] `cargo test -p harness-workflow` passes.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes before PR
      merge readiness.

## Edge Cases

- Tests that rely on `resolve_database_url(None)` must keep their current skip
  behavior when local Postgres is unavailable.
- Async worker and dispatcher tests must keep their existing timing, lease, and
  retry semantics.
- Existing submodules under `runtime/tests/` must keep compiling while the root
  helpers move.
- Test inventory comparison must ignore ordering differences introduced by file
  movement.

## Rollout Notes

This is a mechanical maintainability refactor. The implementation PR should use
`Closes #1462` only after the test inventory diff is empty and the focused
verification commands pass.
