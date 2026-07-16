# Product Spec

## Linked Issue

GH-1637

Complexity: medium. This stabilizes timing-sensitive agent lifecycle tests by
measuring cleanup from the event that actually starts the cleanup contract.

## User Problem

The normal parallel `harness-agents` lib suite intermittently exceeds a
five-second outer timeout in Codex and Claude descendant cleanup tests on a
loaded macOS host. The original timeout begins before the mock agent process is
scheduled and before its descendant-start marker exists, so unrelated process
startup delay is charged to process-group cleanup.

Further evidence disproved the original fixture-serialization hypothesis: two
coordinated package runs passed, the third failed, and the exact Claude test
failed on run 8 of 10. Temporary timing diagnostics then showed that once
`ManagedChild` cleanup started, process groups drained in roughly 0–14 ms,
while total test time varied up to 5.94 seconds under unrelated system load.

## Goals

1. Make the normal parallel `harness-agents` lib suite reliable on the
   reproducing macOS environment.
2. Separate bounded process startup observation from the existing five-second
   root-exit cleanup deadline.
3. Start the cleanup deadline only after the descendant-start marker proves
   the fixture has entered the behavior under test.
4. Preserve all descendant-kill, output, cancellation, and no-late-mutation
   assertions.
5. Restore the GH-1635 real DB-less pre-push gate without skips or global test
   serialization.

## Non-Goals

- Changing `ManagedChild`, process-group signaling, or production agent
  execution behavior.
- Increasing the five-second cleanup deadline.
- Ignoring tests, weakening marker assertions, or accepting late mutation.
- Running the whole crate or workspace with `--test-threads=1`.
- Stopping or reconfiguring unrelated operator processes to manufacture green.
- Changing the GH-1635 database routing implementation.

## User-Visible Behavior

Contributors continue to run the normal parallel test command. Each root-exit
fixture first gives its mock process a separately bounded opportunity to start
its descendant. Once that marker exists, the existing five-second deadline
measures only root exit, process-group cleanup, and pipe drainage.

## Testable Invariants

1. B-001: `cargo test -p harness-agents --lib` passes repeatedly with the
   default test-thread setting on the reproducing macOS environment.
2. B-002: The workspace command used by pre-push passes without
   `--test-threads=1`, ignore flags, or agent-test skip filters.
3. B-003: The Codex streamed, Codex buffered, and Claude streamed root-exit
   fixtures start their five-second completion deadline only after their
   descendant-start marker is observed.
4. B-004: Descendant startup observation is independently bounded and a
   missing marker fails the test rather than removing the cleanup deadline.
5. B-005: The existing five-second root-exit/cleanup deadline remains exactly
   five seconds.
6. B-006: Existing mock scripts, parsed-output checks, descendant-start checks,
   cancellation checks, and no-late-mutation assertions remain semantically
   intact.
7. B-007: No cross-test lock, global serialization, sleep-based retry, skip, or
   ignore is added.
8. B-008: No dependency, production runtime branch, hook bypass, or global
   environment mutation is added.
9. B-009: Three consecutive package runs, the ordinary workspace command, full
   Clippy, and the deterministic GH-1635 hook test pass on the combined head.
10. B-010: After the repair merges and GH-1635 rebases, the real DB-less
    pre-push gate passes with all selected tests enabled.

## Acceptance Criteria

- [ ] B-001 through B-010 have fresh evidence.
- [ ] Three consecutive normal parallel `harness-agents` lib runs pass.
- [ ] The five-second cleanup deadline and final workspace-safety assertions
      are unchanged.
- [ ] The workspace pre-push test command passes in ordinary parallel mode.
- [ ] GH-1635 resumes without `--no-verify`, skips, or global serialization.

## Boundary Checklist

| Category | Coverage |
| --- | --- |
| Empty / missing input | N/A: test fixture timing, not input parsing |
| Error and failure paths | covered: B-004/B-006 preserve missing-marker, cancellation, pipe, and descendant failures |
| Authorization / permission | N/A: no authorization behavior changes |
| Concurrency / race / ordering | covered: B-003 orders marker observation before the cleanup deadline without serializing tests |
| Retry / repetition / idempotency | covered: B-001/B-009 require repeated default-parallel runs |
| Illegal state transitions | N/A: no persisted state machine changes |
| Compatibility / migration | covered: B-008 forbids production and dependency changes |
| Degradation / fallback | covered: B-005/B-007 forbid timeout inflation, skips, retries, and global serialization |
| Evidence and audit integrity | covered: B-009/B-010 require package, workspace, hook, and repeated-run evidence |
| Cancellation / interruption / partial completion | covered: B-004/B-006 keep startup failure and cancellation/drop behavior explicit |

## Edge Cases

- The host is heavily loaded before the mock process runs: startup observation
  has its own bound and does not consume the cleanup deadline.
- The agent task exits before creating the marker: the fixture fails with
  startup/task evidence.
- The descendant starts but cleanup hangs: the unchanged five-second deadline
  still fails.
- The descendant starts and is killed: the final delayed marker assertion
  still proves it cannot mutate the workspace.
- CI has fewer or more test threads: no fixed worker count or global lock is
  required.

## Rollout Notes

GH-1639 must land first so the Claude lifecycle file is below the hard ceiling.
No runtime rollout or migration is needed. Rollback restores the known flaky
delivery gate.
