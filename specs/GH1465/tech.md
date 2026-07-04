# Tech Spec

## Linked Issue

GH-1465

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Task spawn/retry loop | `crates/harness-server/src/task_runner/spawn.rs` | Retry loop tracks `total_turns_used` against the max_turns budget; several paths `break Ok(())` (e.g. workspace-lifecycle failures) | Budget exhaustion and early breaks are the paths that must terminalize |
| Task execution | `crates/harness-server/src/task_executor.rs` (+ extracted modules per #1443) | Drives triage/plan/implement/review phases, emits `status_changed` events | Round accounting source |
| Event emission | task events store (`task-events.jsonl` / events store) | Emits `created`, `status_changed`, `round_completed`, `completed`, `failed` | New terminal reason + audit surface |
| Status surfaces | `crates/harness-server/src/http/` status/overview endpoints (see #1440) | Aggregate task states for dashboard/API | Must classify `stalled` as terminal |
| Recovery/reconciliation | workflow runtime reconciler (see GH-1438), startup recovery | Re-proposes transitions on restart | Restart must not resurrect limbo |

## Proposed Design

1. **Terminal reason enum** — add `stalled` terminal outcome (or `failed`
   with structured reason) carrying `{reason: round_budget_exhausted,
   rounds_used, last_status, waiting_on}` in the event payload.
2. **Budget-exhaustion transition** — in the spawn/executor loop, when
   `total_turns_used >= max_turns` and the task is not `completed`, emit the
   terminal event and persist the state in the same mutation
   (`mutate_and_persist`), before breaking out of the loop.
3. **Close the `break Ok(())` holes** — audit every early `break`/return in
   `spawn.rs` (workspace-lifecycle failure paths included): each must either
   have already recorded a terminal event or do so at the break site. A
   debug assertion (`debug_assert_terminal_on_drop`-style guard) wraps the
   spawn future: on exit, task state must be terminal or explicitly
   re-queued.
4. **Exactly-once terminal** — terminal emission goes through one function
   that compare-and-swaps the persisted status; a second terminal proposal
   becomes a logged no-op.
5. **Liveness audit check** — a post-run audit (test helper + eval gate per
   GH-1447 direction): given an event stream, assert `created` count ==
   terminal count once the queue drains; wire into the workflow regression
   eval and a `checks/`-style deterministic script over `task-events.jsonl`.
6. **Status classification** — status/overview endpoints and the dashboard
   map `stalled` into the terminal bucket with its reason visible.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 exactly-one terminal | terminal CAS function | unit test: double-terminal proposal → single event |
| P2 budget exhaustion terminalizes | spawn/executor loop | integration test: turn cap 2, review never passes → `stalled(round_budget_exhausted)` |
| P3 error-level visibility | event emission | test asserts error-level event + reason payload |
| P4 status surfaces terminal | http status endpoints | endpoint test: stalled task counted terminal |
| P5 crash safety | recovery path | restart-mid-review test: task resumes or terminalizes |

## Data Flow

Executor round accounting → budget check → terminal CAS → persisted task
state + terminal event → status API/dashboard + audit check over the event
stream.

## Alternatives Considered

- Timeout-based sweeper that marks old `waiting` tasks stalled — rejected as
  primary fix: it papers over the missing transition and misfires on
  legitimately long waits; acceptable later as a belt-and-braces reconciler
  rule.
- Treating budget exhaustion as `completed` with a warning — rejected:
  silent degradation (U-29); the work is not done.
- Only fixing the dashboard query — rejected: state must be truthful at the
  source, not reinterpreted per consumer.

## Risks

- Behavior change: runs that previously "ended quietly" now produce failed/
  stalled terminals — downstream automation keying on `failed` may retrigger;
  mitigated by the distinct `stalled` reason.
- Race between completion and exhaustion — covered by the CAS single-writer
  design and a dedicated test.
- The April-era records predate the current workflow runtime; the audit
  check is the guard that proves the invariant on today's code rather than
  assuming the old bug shape.

## Test Plan

- [ ] Unit: terminal CAS exactly-once; reason payload shape.
- [ ] Integration: turn-cap exhaustion → stalled; restart-mid-review; shutdown drain.
- [ ] Audit: event-stream liveness check green on full workspace test suite; wired into regression eval.
