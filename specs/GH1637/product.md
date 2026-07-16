# Product Spec

## Linked Issue

GH-1637

Complexity: medium. This stabilizes timing-sensitive agent lifecycle tests
without changing production child-process behavior or weakening workspace
safety assertions.

## User Problem

The normal parallel `harness-agents` lib suite intermittently exceeds the
five-second deadlines in Codex descendant cleanup tests on macOS. The exact
tests pass alone, the three root-exit descendant tests pass together, and the
complete 193-test binary passes with `--test-threads=1`. Two consecutive real
pre-push runs nevertheless failed, blocking GH-1635 and making the local gate
unreliable.

The affected fixtures deliberately leave children or descendants alive while
testing root exit, pipe drainage, cancellation, and drop cleanup. Running all
of those destructive lifecycle fixtures concurrently adds process reaping and
scheduler contention to assertions that are intended to test one cleanup
contract at a time.

## Goals

1. Make the normal parallel `harness-agents` lib suite reliable on the
   reproducing macOS environment.
2. Coordinate only lifecycle fixtures that intentionally leave long-running
   child or descendant processes.
3. Preserve every bounded-return and no-late-workspace-mutation assertion.
4. Keep all unrelated tests parallel and add no dependency.
5. Restore the GH-1635 real DB-less pre-push gate without a global serial-test
   flag or test skip.

## Non-Goals

- Changing `ManagedChild`, process-group signaling, or production agent
  execution behavior without independent production-failure evidence.
- Increasing the five-second root-exit deadlines as the fix.
- Ignoring tests, weakening marker assertions, or accepting late mutation.
- Running the whole crate or workspace with `--test-threads=1`.
- Changing the GH-1635 database routing implementation.

## User-Visible Behavior

Contributors continue to run the normal parallel test command. The test binary
internally coordinates the small set of destructive process lifecycle
fixtures, while ordinary unit tests remain parallel. Failures in agent cleanup
still fail the suite with the existing assertions and deadlines.

## Testable Invariants

1. B-001: `cargo test -p harness-agents --lib` passes repeatedly with the
   default test-thread setting on the reproducing macOS environment.
2. B-002: The workspace command used by pre-push passes without adding
   `--test-threads=1`, an ignore flag, or a skip filter for agent tests.
3. B-003: Codex and Claude lifecycle fixtures share one test-only coordination
   primitive rather than independent module locks.
4. B-004: Each coordinated fixture acquires the guard before spawning its
   child and holds it through its final descendant/no-mutation assertion.
5. B-005: Existing timeouts, descendant-start checks, parsed-output checks,
   cancellation checks, and no-late-mutation assertions remain unchanged.
6. B-006: The coordination primitive is async-aware and safe to hold across
   `await`; no blocking mutex guard is held across an async suspension point.
7. B-007: Tests that do not create long-lived child/descendant lifecycle
   conditions remain uncoordinated and parallel.
8. B-008: No dependency, production runtime branch, hook bypass, or global
   environment mutation is added.
9. B-009: Package tests, full workspace Clippy, and the deterministic GH-1635
   hook contract test pass on the implementation head.
10. B-010: After the fix is merged and GH-1635 is rebased, the real DB-less
    pre-push gate passes with all selected tests enabled.

## Acceptance Criteria

- [ ] B-001 through B-010 have fresh evidence.
- [ ] Three consecutive normal parallel `harness-agents` lib runs pass.
- [ ] The existing process cleanup assertion bodies and timeout values have no
      semantic weakening.
- [ ] The workspace pre-push test command passes in its ordinary parallel mode.
- [ ] GH-1635 can resume without `--no-verify`, skips, or global serialization.

## Boundary Checklist

| Category | Coverage |
| --- | --- |
| Empty / missing input | N/A: this is test-fixture scheduling, not input parsing |
| Error and failure paths | covered: B-005 preserves cancellation, timeout, pipe, and descendant failure assertions |
| Authorization / permission | N/A: no external authorization behavior changes |
| Concurrency / race / ordering | covered: B-003/B-004 define one cross-adapter critical section and its lifetime |
| Retry / repetition / idempotency | covered: B-001 requires repeated default-parallel runs |
| Illegal state transitions | N/A: no persisted state machine changes |
| Compatibility / migration | covered: B-008 forbids production and dependency changes |
| Degradation / fallback | covered: B-002/B-005 forbid skips, ignores, global serialization, and timeout inflation |
| Evidence and audit integrity | covered: B-009/B-010 require package, workspace, hook, and repeated-run evidence |
| Cancellation / interruption / partial completion | covered: B-004/B-005 retain the cancellation/drop fixtures and hold coordination through cleanup assertions |

## Edge Cases

- A coordinated test panics: the async mutex releases with the guard and does
  not poison subsequent tests.
- Codex and Claude lifecycle tests run on different libtest worker threads:
  they still use the same crate-level primitive.
- A fixture fails before spawning: guard cleanup remains automatic.
- A descendant starts but cleanup fails: the existing marker assertion still
  fails; coordination must not convert it into success.
- CI has fewer or more test threads than the reproducing Mac: the contract does
  not depend on a fixed worker count.

## Rollout Notes

Land this repair before GH-1635 implementation readiness. No runtime rollout or
migration is needed. Rollback reverts the implementation PR and restores the
known flaky delivery gate.
