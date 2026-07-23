# Product Spec

## Linked Issue

GH-1716

## User Problem

When the server starts, retained legacy task rows are repaired from the event
log and task checkpoints. Those recovery writes use optimistic-lock version
predicates, but a concurrent writer can advance the row between the recovery
read and write. The resulting zero-row update is currently counted and logged
as successful. Event replay can also compact terminal evidence even though the
corresponding durable task update never happened.

Operators therefore receive false recovery evidence, while a still-eligible
task can remain unrecovered without an explicit error.

## Goals

- Classify every version-guarded legacy recovery write as `Applied`,
  `Superseded`, or `Conflict`.
- Count and log only durable writes that were actually applied.
- Require fresh durable evidence before treating a lost CAS as superseded.
- Fail closed on unresolved conflicts and preserve unapplied terminal replay
  evidence.
- Preserve the existing checkpoint resume, interrupted-task failure, and
  transient-retry failure policies.

## Non-Goals

- Changing workflow runtime states, decisions, commands, recovery APIs, or
  persistence.
- Reintroducing legacy task submission or background recovery loops.
- Adding a database migration or changing the `tasks.version` contract.
- Automatically retrying a lost CAS against a row that may have a live writer.
- Redesigning `TaskStatus` transitions or the broader legacy task layer.

## User-Visible Behavior

1. **B-001:** Every version-guarded event-replay or startup-recovery write
   produces exactly one closed outcome: `Applied`, `Superseded`, or
   `Conflict`.
2. **B-002:** Only `Applied`, proven by exactly one affected row, increments
   `updated`, `resumed`, `failed`, or `transient_failed`, and only `Applied`
   may emit success wording for that action.
3. **B-003:** A zero-row write is `Superseded` only after a fresh authoritative
   read proves that the task row is absent, already contains the intended
   durable result, or no longer satisfies that recovery action's eligibility
   predicate.
4. **B-004:** A zero-row write is `Conflict` when the fresh row still satisfies
   the action's eligibility predicate but does not contain the intended
   durable result. The conflict is an explicit error that identifies the task,
   recovery action, expected version, and current version when available.
5. **B-005:** Event replay increments its applied-task count only for
   `Applied`. Any unresolved `Conflict` aborts replay before terminal event-log
   compaction, so unapplied terminal evidence remains available for a later
   recovery attempt.
6. **B-006:** Recovery does not blindly retry a `Conflict`. Re-running recovery
   after the competing writer has quiesced is idempotent: an equivalent result
   is `Superseded`, while an eligible unapplied row receives a new CAS attempt
   from its current version.
7. **B-007:** Database, serialization, and scheduler-state decoding failures
   remain explicit errors and are never reclassified as `Superseded` or
   counted as successful recovery.
8. **B-008:** In the absence of a CAS conflict, recovery preserves existing
   policy: an interrupted task with a usable PR, plan, or triage checkpoint
   becomes pending for resume; an interrupted task without one becomes
   failed; a pending task interrupted during transient retry becomes failed.
9. **B-009:** Startup aggregate logs reflect only durable `Applied` outcomes.
   `Superseded` may emit non-success diagnostic evidence, and `Conflict` may
   not be hidden by an aggregate success message.
10. **B-010:** The change applies only to retained legacy `TaskDb` startup and
    event-replay recovery. Canonical workflow runtime behavior and artifacts
    remain unchanged.

## Acceptance Criteria

- [ ] B-001 through B-010 have deterministic implementation and test evidence.
- [ ] All six version-guarded recovery UPDATE sites inspect their affected-row
      result.
- [ ] Applied, superseded, and conflicting interleavings are covered against
      PostgreSQL using stale-version fixtures or controlled concurrent actors.
- [ ] Recovery counters and success logs exclude superseded and conflicting
      writes.
- [ ] A replay conflict leaves terminal JSONL evidence uncompacted.
- [ ] Existing checkpoint, no-checkpoint, PR writeback, terminal replay, and
      transient-retry recovery behavior remains covered.
- [ ] No schema, workflow runtime, public HTTP API, or authorization behavior
      changes.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-003 and B-007: a missing row is explicit supersession, while malformed durable state remains an error. |
| Error and failure paths | Covered by B-004, B-005, and B-007. |
| Authorization / permission | N/A: startup recovery operates on already-authorized local durable state and introduces no user action or external permission check. |
| Concurrency / race / ordering | Covered by B-001 through B-005. |
| Retry / repetition / idempotency | Covered by B-006. |
| Illegal state transitions | Covered by B-003 and B-008: a no-longer-eligible row is not overwritten, and the existing recovery state policy is unchanged. |
| Compatibility / migration | Covered by B-008 and B-010; existing rows remain compatible and no schema migration is added. |
| Degradation / fallback | Covered by B-002, B-004, B-005, and B-009; a lost CAS cannot look like success. |
| Evidence and audit integrity | Covered by B-002, B-005, and B-009. |
| Cancellation / interruption / partial completion | Covered by B-005 and B-006: replay evidence survives unresolved conflict and a later invocation can recover safely. |

## Edge Cases

- The row is deleted after recovery selects it but before the UPDATE.
- Another writer applies the same target status or PR URL first.
- Another writer advances the row to a state that is no longer eligible for
  the selected recovery action.
- Another writer bumps the version but leaves the row eligible and without the
  intended recovery result.
- Event replay contains both a terminal status and a PR URL.
- A transient-retry row changes status or error classification during
  recovery.
- Several tasks recover successfully before a later task conflicts.
- Scheduler-state JSON is malformed before or after a competing write.

## Rollout Notes

No migration or operator configuration is required. The change intentionally
makes unresolved startup conflicts louder and may cause critical legacy task
storage startup to fail until the competing writer is stopped or the next
recovery attempt observes a superseded result. Reverting the implementation
restores the previous behavior but also restores false recovery counts and
unsafe replay compaction.
