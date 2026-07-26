# Product Spec

## Linked Issue

GH-1772

## User Problem

The workflow runtime can silently lose finished agent work: when a worker's
job lease expires mid-turn, the completed activity result is discarded with a
log line while the agent process and its workspace lease keep running, and a
second worker re-runs the same activity against a potentially dirty tree.
Separately, the runtime's safety loops (retention, watchdog, schema reaping)
default off or fail silent on config errors while dispatch keeps executing,
so unbounded table growth and stuck workflows are the default operating
posture. `WorkflowInstance.version` looks like a concurrency guard but is
never checked, inviting a future silent-lost-update bug.

Operators need completion, ownership, and background maintenance to fail
closed and visibly: no finished result dropped without a durable record, no
workspace occupied by a job that lost ownership, no safety loop silently
disabled while agents continue to spend.

## Goals

- Persist every lease-rejected activity completion as a durable dead-letter
  record instead of dropping it.
- Bind runtime job ownership and workspace ownership to one epoch so the two
  lease systems cannot silently diverge.
- Resolve `WorkflowInstance.version` into either an enforced concurrency
  predicate or an explicitly informational field — not a decoy.
- Ship safe retention defaults with an operator-visible dry-run before any
  destructive action.
- Supervise background loops and make `WORKFLOW.md` parse failure a single,
  loud, health-surfaced degraded state.

## Non-Goals

- Multi-instance orchestration or leader election (design note only; no
  implementation).
- Schema migration of existing lease tables beyond additive columns.
- Changes to workflow state graphs, transition allowlists, or lifecycle
  semantics.
- Changes to agent spawning, prompt construction, or review/merge gates.
- Replacing the pessimistic row-lock transaction model for instance mutation.

## User-Visible Behavior

1. **B-001:** A completion rejected because the worker no longer owns the job
   lease is persisted as a dead-letter completion record carrying the job id,
   owner, ownership epoch, full activity result, and transcript reference,
   and emits a runtime event. It is never reported as success and never
   silently dropped.
2. **B-002:** If the same activity has not been re-claimed when the
   dead-letter record is written, reconciliation may adopt the dead-lettered
   result through the normal completion path exactly once; adoption is
   idempotent and recorded as an event. If a re-run is already active, the
   record is retained for operator comparison and is not auto-adopted.
3. **B-003:** Claiming a runtime job mints an ownership epoch, and any
   workspace lease acquired for that job records the same epoch. Workspace
   lease renewal under a stale epoch is rejected.
4. **B-004:** When a job lease expires or is reclaimed, the workspace lease
   held under the same epoch becomes releasable: it is released as soon as
   the owning agent process is observed exited, and a re-claiming worker
   never receives a workspace slot still writable by the previous epoch's
   live process.
5. **B-005:** Every UPDATE of a workflow instance row either includes the
   instance version as a predicate and treats zero rows affected as a typed
   conflict error, or the field is renamed/documented as informational with
   no concurrency semantics. Exactly one of the two outcomes ships; the
   current ambiguous state is removed.
6. **B-006:** Runtime and task retention are enabled by default with
   conservative terminal-only scope: only workflows in a terminal state older
   than the configured age are eligible, and event/decision/transcript rows
   for non-terminal workflows are never touched.
7. **B-007:** The first retention activation for a store runs in dry-run
   mode: at least one full interval reports would-delete counts and byte
   estimates as runtime events before destructive mode engages. Dry-run
   results are visible to operators without database access.
8. **B-008:** Each background loop registers with a supervisor that records
   its last successful tick. The health endpoint reports per-loop staleness,
   and a loop that misses its staleness threshold flips the health surface to
   degraded, naming the loop.
9. **B-009:** A `WORKFLOW.md` parse failure is reported once per transition
   at error level, is visible on the health endpoint, and marks every
   config-gated loop as degraded rather than each loop silently retrying. No
   config-failure arm may skip logging.
10. **B-010:** While config parse failure persists beyond a configurable
    grace period, new runtime dispatch is paused until the config loads
    again; already-running activities are not interrupted.
11. **B-011:** All new records and events introduced here (dead-letter
    completions, epoch fields, dry-run reports, supervisor status) are
    additive; existing rows, wire formats, and workflow semantics remain
    readable and unchanged.

## Acceptance Criteria

- [ ] A store test proves a lease-expired completion writes a dead-letter
      record with result and transcript reference and emits the event,
      instead of returning silently.
- [ ] A reconciliation test proves single adoption of a dead-lettered result
      when no re-claim occurred, idempotent on repeat, and non-adoption when
      a re-run is active.
- [ ] A lease test proves workspace renewal under a stale epoch is rejected,
      and a re-claiming worker cannot obtain a slot while the prior epoch's
      process is still live.
- [ ] Either conflict-on-stale-version store tests exist for every instance
      UPDATE path, or the field is renamed/documented and a lint/test guards
      against reintroducing version-as-predicate assumptions — matching
      whichever resolution B-005 selects.
- [ ] Retention tests prove terminal-only eligibility, age gating, and that
      dry-run mode deletes nothing while reporting accurate counts.
- [ ] A supervisor test proves a stalled loop surfaces as degraded health
      naming the loop, and a recovered loop clears it.
- [ ] A config-failure test proves one error-level report, degraded health,
      all config-gated loops marked degraded (including the orphan reaper's
      previously silent arm), and dispatch pausing after the grace period.
- [ ] Existing lease, recovery, and retention tests continue to pass without
      semantic weakening.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-009/B-010: missing or unparseable config is a loud degraded state, not a silent skip. |
| Error and failure paths | Covered by B-001, B-008, B-009; no warn-and-drop for completed work, no unlogged failure arms. |
| Authorization / permission | N/A. Ownership epochs constrain internal workers, not external caller authority. |
| Concurrency / race / ordering | Covered by B-002, B-003, B-004, B-005. |
| Retry / repetition / idempotency | Covered by B-002 (single adoption, idempotent repeat) and B-005 (typed conflict on stale write). |
| Illegal state transitions | Covered by B-004 (stale-epoch renewal rejected) and B-005. |
| Compatibility / migration | Covered by B-011: additive columns and records only. |
| Degradation / fallback | Covered by B-007, B-009, B-010: degradation is visible and pauses spend, never silent. |
| Evidence and audit integrity | Covered by B-001, B-002, B-007: dropped work and would-delete actions leave durable records. |
| Cancellation / interruption / partial completion | Covered by B-001, B-004, B-010: interrupted ownership yields dead-letter plus releasable workspace, and running activities are not killed by config pauses. |

## Edge Cases

- A worker finishes its turn seconds after lease expiry while the re-claiming
  worker has claimed but not yet started the activity.
- The agent process outlives both its job lease and its worker, still holding
  the workspace slot.
- A dead-letter adoption races the re-run's own completion commit.
- Retention dry-run reports rows that a concurrent recovery action then
  makes non-terminal again before destructive mode engages.
- `WORKFLOW.md` alternates between valid and invalid across consecutive
  ticks (flapping), which must not spam error logs or flap dispatch pausing
  within the grace period.
- Two server processes point at the same schema while singleton loops run
  (documented hazard; supervisor design keeps singleton loops behind an
  advisory-lock acquisition as a forward-compatibility note).

## Rollout Notes

All schema changes are additive (epoch columns, dead-letter table, supervisor
status). Retention flips from off to dry-run-by-default in the first release
and to destructive-by-default only after the dry-run interval completes per
store, giving operators one full cycle of visibility. Reverting restores the
current behavior, including the silent-drop path; revert should therefore be
paired with disabling lease-expiry-sensitive workloads or accepting the
documented loss window.
