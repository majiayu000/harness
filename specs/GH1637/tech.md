# Tech Spec

## Linked Issue

GH-1637

## Product Spec

`specs/GH1637/product.md`

## Codebase Context

Updated from the original `7b8dc534` baseline after GH-1639 was specified.

| Area | Anchor | Current behavior | Relevance |
| --- | --- | --- | --- |
| Child cleanup | `crates/harness-agents/src/lib.rs:151-357` | `ManagedChild` bounds post-exit group drainage at two seconds. Diagnostic runs drained in 0–14 ms once cleanup began. | Production cleanup is not the observed delay and remains unchanged. |
| Codex lifecycle fixtures | `crates/harness-agents/src/codex_tests.rs:291-489` | Two root-exit tests wrap process startup and cleanup together in five seconds; adjacent cancellation/drop tests already observe startup separately. | Reuse the proven two-phase fixture pattern. |
| Claude lifecycle fixtures | `crates/harness-agents/src/claude_tests/lifecycle.rs` after GH-1639 | The streamed root-exit test has the same combined deadline; cancellation/drop tests already separate startup observation. | Apply the same marker-anchored contract after the split. |
| Startup helpers | `codex_tests.rs:88-163`, `claude_tests.rs:19-30` | Existing helpers observe stream/task progress or filesystem markers with independent bounds. | Reuse rather than adding synchronization infrastructure. |
| GH-1635 hook | `.githooks/pre-push` in the GH-1635 implementation branch | Runs the ordinary parallel non-server/non-workflow workspace lib suite. | Must pass without whole-suite serialization. |

## Root-Cause Evidence

The original parallel suite failed twice. A shared async lock around seven
lifecycle fixtures produced two passing package runs and then another exact
timeout, falsifying fixture concurrency as the root cause. The exact Claude
root-exit test then failed on run 8 of 10.

Temporary test-only timing diagnostics ran the exact fixture 20 times. Once
`ManagedChild` cleanup started, group drainage completed in approximately
0–14 ms. Total fixture duration varied from about 2.05 to 5.94 seconds while an
unrelated Cargo build kept host load high. The delay therefore occurs before
the descendant marker and cleanup phase, but the existing timeout charges that
startup/scheduling delay to cleanup.

## Proposed Design

### 1. Land the GH-1639 prerequisite

First merge the behavior-preserving extraction of the three Claude lifecycle
fixtures into `claude_tests/lifecycle.rs`. GH-1637 then edits compliant files
without bypassing the 800-line hard ceiling.

### 2. Run root-exit fixtures as observable tasks

For the three tests that prove a root process exits while a descendant holds a
pipe:

- spawn the existing agent execution future as a Tokio task;
- independently observe the existing descendant-start marker with a bounded
  startup helper;
- fail and abort cleanly when the marker is not observed;
- only after the marker exists, wrap the task handle in the existing
  five-second completion timeout.

The Codex streamed test can reuse
`assert_path_observed_before_task_exit`. The buffered Codex and streamed Claude
tests may use their existing marker polling pattern with explicit abort/join
diagnostics appropriate to each task result type.

### 3. Preserve the cleanup contract

Do not change the five-second completion deadline, mock shell scripts, marker
paths, response parsing, descendant-start assertions, 1.5-second
no-late-mutation delay, cancellation/drop tests, or production code.

No shared lock is added. Startup observation is not a retry; it defines the
precondition from which cleanup latency is measured.

## Data Flow

```text
parallel libtest worker
  -> spawn existing mock agent task
  -> bounded startup observation
       -> marker absent/task exits: fail with diagnostics
       -> descendant marker exists: start existing 5-second deadline
  -> await root exit + process-group cleanup + pipe drainage
  -> assert parsed result
  -> retain delayed no-workspace-mutation assertion
```

## Product-to-Test Mapping

| Invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001/B-002 repeated stability | three root-exit fixtures | three package runs plus ordinary workspace command |
| B-003 marker-anchored deadline | Codex streamed/buffered and Claude streamed tests | targeted source review and exact filters |
| B-004 startup failure | existing marker/task helpers and abort branches | missing-marker diagnostics review and targeted tests |
| B-005 unchanged deadline | three `Duration::from_secs(5)` calls | exact diff review |
| B-006 assertion integrity | existing scripts and assertions | semantic diff review |
| B-007 no serialization | crate/test runner configuration | absence of lock, skip, retry, and thread override |
| B-008 no production/dependency change | manifests and non-test code | exact file-list review |
| B-009 local readiness | package/workspace/hook/Clippy commands | fresh command outputs |
| B-010 GH-1635 recovery | rebased GH-1635 branch | real DB-less pre-push run |

## Risks

- Task ownership: spawning changes error shape. Join errors and agent errors
  must remain distinguishable in assertions.
- False green: starting the five-second deadline before the marker would retain
  the bug; starting it after cleanup would weaken the test. Anchor immediately
  after marker observation.
- Hung startup: use an independent bounded observation and abort the task on
  failure.
- Scope drift: do not edit production cleanup or the cancellation/drop tests.
- Platform variance: keep default parallel test commands and do not stop
  unrelated operator workloads to make verification pass.

## Alternatives Considered

1. Shared async test lock. Rejected by new evidence: two runs passed and the
   third failed; the exact test also failed without fixture concurrency.
2. Increase the five-second timeout. Rejected: it would hide phase conflation
   rather than measure cleanup from its real precondition.
3. Global `--test-threads=1`. Rejected: slows unrelated tests and does not fix
   the exact-test failure.
4. Change `ManagedChild`. Rejected: instrumentation shows cleanup itself drains
   promptly once reached.
5. Stop unrelated builds. Rejected: external operator state is out of scope and
   tests must tolerate ordinary host load.

## Test Plan

- [ ] Merge GH-1639 and rebase GH-1637 onto current `origin/main`.
- [ ] `cargo fmt --all -- --check`.
- [ ] Run the three root-exit filters repeatedly.
- [ ] Run `cargo test -p harness-agents --lib` three consecutive times with no
      test-thread override.
- [ ] `cargo test --workspace --exclude harness-server --exclude harness-workflow --lib`.
- [ ] `bash scripts/test-pre-push-hook.sh` on the rebased GH-1635 branch.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Both SpecRail workflow checks.
- [ ] Exact diff review proving the five-second deadline, scripts, final
      assertions, manifests, hooks, and production cleanup are unchanged.

## Rollback Plan

Revert the implementation PR. No production or data rollback is required; the
known loaded-host timing flake and GH-1635 gate blocker would return.
