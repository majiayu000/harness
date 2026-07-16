# Product Spec

## Linked Issue

GH-1639

Complexity: low. This is a behavior-preserving test-module extraction required
to satisfy the repository's file-size hard ceiling.

## User Problem

`crates/harness-agents/src/claude_tests.rs` is 928 lines, above the 800-line
hard ceiling. VibeGuard therefore blocks even the scoped one-line test
coordination changes approved by GH-1637. The file combines argument, model,
parser, provider-capacity, execution, cancellation, and descendant-cleanup
fixtures.

The final three tests form a cohesive lifecycle group and occupy enough lines
to make both the source module and a focused extracted module compliant.

## Goals

1. Reduce `claude_tests.rs` below 800 lines.
2. Extract the three Claude child/descendant lifecycle fixtures into one
   focused test submodule.
3. Preserve all scripts, timeouts, assertions, helper behavior, and production
   code.
4. Unblock GH-1637 without bypassing VibeGuard.

## Non-Goals

- Changing process cleanup behavior or test expectations.
- Adding GH-1637's coordination guard in this PR.
- Reorganizing Codex tests, parser tests, or provider-capacity tests.
- Adding a dependency, public API, hook change, or test-runner flag.

## User-Visible Behavior

There is no runtime behavior change. Test names gain the focused `lifecycle`
module segment, while their bodies and failure conditions remain the same.

## Testable Invariants

1. B-001: `claude_tests.rs` is below 800 lines after extraction.
2. B-002: The extracted `claude_tests/lifecycle.rs` is below 800 lines and
   contains exactly the root-exit, receiver-drop, and timeout-drop fixtures.
3. B-003: The parent module registers the focused submodule through the normal
   Rust module system.
4. B-004: Moved fixture scripts, timeout values, marker assertions,
   cancellation assertions, and cleanup assertions are semantically unchanged.
5. B-005: Existing parent-private helpers and imports are reused without
   duplicate implementations.
6. B-006: No non-test Rust file, Cargo manifest, dependency, hook, or workflow
   changes.
7. B-007: Targeted lifecycle tests and the full `harness-agents` package pass.
8. B-008: The resulting layout accepts GH-1637's scoped guard additions
   without exceeding the hard ceiling.

## Acceptance Criteria

- [ ] B-001 through B-008 have fresh evidence.
- [ ] Both test files are below 800 lines.
- [ ] A move-aware diff shows no semantic change to the three fixtures.
- [ ] `cargo test -p harness-agents --lib` and full workspace Clippy pass.
- [ ] GH-1637 can rebase and proceed without guard bypasses.

## Boundary Checklist

| Category | Coverage |
| --- | --- |
| Empty / missing input | N/A: source-layout-only change |
| Error and failure paths | covered: B-004 preserves every existing failure assertion |
| Authorization / permission | N/A: no authorization behavior |
| Concurrency / race / ordering | covered: B-004 preserves lifecycle ordering; coordination remains GH-1637 scope |
| Retry / repetition / idempotency | covered: B-007 reruns package tests after extraction |
| Illegal state transitions | N/A: no state machine |
| Compatibility / migration | covered: B-003/B-005 use ordinary private test-module nesting |
| Degradation / fallback | covered: B-004/B-006 forbid skips or behavior changes |
| Evidence and audit integrity | covered: line counts, move-aware diff, package tests, and Clippy |
| Cancellation / interruption / partial completion | covered: the receiver-drop and timeout-drop fixtures move intact |

## Rollout Notes

Merge before GH-1637 implementation. No runtime rollout, data migration, or
release note is required.
