# Tech Spec

## Linked Issue

GH-1637

## Product Spec

`specs/GH1637/product.md`

## Codebase Context

Verified at `origin/main` commit `7b8dc534`.

| Area | Anchor | Current behavior | Relevance |
| --- | --- | --- | --- |
| Shared child management | `crates/harness-agents/src/lib.rs:151-357` | `ManagedChild` owns process-group cleanup for both agent adapters. | Production semantics are shared and must remain unchanged without separate failure evidence. |
| Codex lifecycle fixtures | `crates/harness-agents/src/codex_tests.rs:291-489` | Root-exit, receiver-drop, timeout-drop, and buffered execute fixtures create long-lived children or descendants. | These fixtures compete under the full parallel test binary. |
| Claude lifecycle fixtures | `crates/harness-agents/src/claude_tests.rs:761-928` | Equivalent stream root-exit, receiver-drop, and timeout-drop fixtures exercise the same OS lifecycle boundary. | Coordination must cross adapter modules. |
| Existing environment locks | `codex_tests.rs:8-52`, `claude_tests.rs:352-389` | Each module has a private blocking mutex only for process-global environment mutation. | They cannot coordinate cross-adapter async lifecycle tests and must not be reused across `await`. |
| GH-1635 hook command | `.githooks/pre-push` in the GH-1635 implementation branch | Runs the ordinary parallel non-server/non-workflow workspace lib suite. | The repair must make this real gate pass without serializing the whole workspace. |

## Reproduction Evidence

Two consecutive ordinary workspace test runs failed in one or both Codex
root-exit descendant tests at their five-second outer timeout. Each exact test
passed alone, all three root-exit descendant tests passed together, and the
complete 193-test binary passed with `--test-threads=1` in 40.03 seconds. This
isolates the failure to full-suite fixture concurrency rather than the GH-1635
database environment or a deterministic single-cleanup failure.

## Proposed Design

### 1. Add one crate-level async test coordination primitive

Under `#[cfg(test)]` in `crates/harness-agents/src/lib.rs`, add a crate-private
async helper backed by `OnceLock<tokio::sync::Mutex<()>>`. The helper returns a
`tokio::sync::MutexGuard<'static, ()>`.

This location gives Codex and Claude tests one lock. Tokio's mutex is designed
for guards held across `await`, avoids blocking a libtest worker, and does not
poison after a panic. Tokio is already a workspace dependency.

Do not add a public API, production branch, dependency, environment variable,
or global test-runner setting.

### 2. Coordinate only destructive lifecycle fixtures

Acquire the shared guard as the first statement in these tests and retain it
through the final cleanup/no-mutation assertion:

- Codex root exit with descendant-held stderr;
- Codex receiver-drop cancellation with a long-lived child;
- Codex timeout/drop with a descendant;
- Codex buffered execute with descendant-held stdout;
- Claude root exit with descendant-held stderr;
- Claude receiver-drop cancellation;
- Claude timeout/drop with a descendant.

Do not coordinate parser, argument, registry, ordinary short-process, or other
unit tests. Coordination wait time occurs before each fixture's existing
operation-specific timeout begins.

### 3. Preserve the safety assertions verbatim

The implementation adds guard acquisition only. It does not change fixture
scripts, sleep durations, timeout values, output assertions, descendant-start
markers, cancellation expectations, or final no-mutation markers.

The lock prevents multiple destructive fixtures from competing for orphan
reaping and scheduler time; it does not turn a failed cleanup into success.

## Data Flow

```text
normal parallel libtest run
  -> ordinary tests continue in parallel
  -> lifecycle fixture awaits shared async guard
       -> spawn child/descendant
       -> execute existing timeout/cancellation contract
       -> assert descendant started
       -> assert no late workspace mutation
       -> release guard
  -> next lifecycle fixture proceeds
```

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001/B-002 repeated default-parallel stability | coordinated lifecycle fixture set | three `cargo test -p harness-agents --lib` runs plus the workspace pre-push command |
| B-003 shared cross-adapter primitive | `harness-agents/src/lib.rs` test-only helper | code review and both adapter test modules using the same helper |
| B-004 guard lifetime | seven named fixtures | diff review confirms first-statement acquisition and final-assertion scope |
| B-005 assertion integrity | existing Codex/Claude test bodies | semantic diff review; no timeout/script/assertion changes |
| B-006 async safety | Tokio mutex helper | Clippy with `-D warnings`; no `std::sync::MutexGuard` across `await` |
| B-007 scoped concurrency | all other agent tests | diff boundary and ordinary parallel suite |
| B-008 no production/dependency change | Cargo manifests and non-test code | exact diff review |
| B-009 local readiness | package/workspace commands | fmt, package tests, hook contract test, full Clippy |
| B-010 GH-1635 recovery | rebased GH-1635 branch | real DB-less pre-push run after merge |

## Risks

- Coverage: over-broad coordination could hide unrelated races. Limit use to
  the seven fixtures that intentionally retain long-lived processes.
- Deadlock: a blocking mutex across async suspension could stall worker
  threads. Use `tokio::sync::Mutex`, acquire once, and rely on guard drop.
- False green: changing timeout or marker assertions would weaken the safety
  contract. The implementation diff must contain guard additions only in the
  seven test bodies.
- Platform behavior: macOS exposed the contention, while Linux CI may not.
  Keep ordinary default-parallel commands on both platforms.
- Panic recovery: Tokio mutexes do not poison, so one failed test does not
  cascade lock acquisition failures.

## Alternatives Considered

1. Raise five-second timeouts. Rejected: it hides contention and weakens the
   bounded-return contract.
2. Run the entire crate/workspace with `--test-threads=1`. Rejected: it slows
   unrelated tests and masks broader concurrency regressions.
3. Add `serial_test`. Rejected: a standard-library/Tokio solution already
   exists and a new dependency is unnecessary.
4. Reuse module environment locks. Rejected: they are blocking, adapter-local,
   and unsafe to hold across async suspension.
5. Change `ManagedChild` signaling or drain semantics. Rejected for this issue:
   exact and serial runs pass, so current evidence does not justify changing a
   workspace-safety production path.

## Test Plan

- [ ] Preserve the original failing outputs as baseline evidence.
- [ ] `cargo fmt --all -- --check`.
- [ ] Run `cargo test -p harness-agents --lib` three consecutive times with no
      test-thread override.
- [ ] `cargo test --workspace --exclude harness-server --exclude harness-workflow --lib`.
- [ ] `bash scripts/test-pre-push-hook.sh` on the rebased GH-1635 branch.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `python3 checks/check_workflow.py --repo .`.
- [ ] `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1637`.
- [ ] Exact diff review proving no timeout, script, assertion, manifest, hook,
      or production cleanup change.

## Rollback Plan

Revert the implementation PR. No production process, data, migration, or API
rollback is needed. The original parallel-suite flake and GH-1635 gate blocker
would return.
