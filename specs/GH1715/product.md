# Product Spec

## Linked Issue

GH-1715

## User Problem

The retained legacy issue-workflow compatibility path accepts nearly every
lifecycle event from every source state. A late or duplicated event can reopen
a terminal workflow, overwrite its task or pull-request metadata, and leave
GitHub intake treating the corrupted row as authoritative coverage.

Operators need legacy lifecycle updates to fail closed while preserving the
intentional recovery and repetition behavior that existing issue and pull
request processing depends on.

## Goals

- Define one complete, explicit transition contract for all 14
  `IssueLifecycleState` values and all 16 `IssueLifecycleEventKind` values.
- Reject every unlisted state/event pair with a visible typed error.
- Preserve intentional idempotency, feedback-claim recovery, and
  server-authoritative terminal reconciliation.
- Ensure a rejected persisted update leaves the complete workflow instance
  unchanged.

## Non-Goals

- Merging the legacy issue lifecycle with the canonical workflow runtime.
- Treating SpecRail governance states as Rust runtime states.
- Adding, removing, or renaming lifecycle states or events.
- Changing event payload schemas, persistence schemas, or serialized wire tags.
- Redesigning `WorkflowRuntimeStore`, `TaskStatus`, intake, reconciliation, or
  server orchestration.
- Adding a new repository abstraction around `IssueWorkflowStore`.

## User-Visible Behavior

1. **B-001:** Every state/event pair is classified by one explicit,
   fail-closed transition contract. Any pair not listed in the contract is
   illegal.
2. **B-002:** An illegal transition returns a typed error and leaves the
   complete in-memory workflow unchanged, including state, task and PR
   metadata, `last_event`, remote-fact metadata, and timestamps.
3. **B-003:** `Done`, `Failed`, and `Cancelled` cannot be reopened by ordinary
   lifecycle events. Repeating the matching terminal event is idempotent;
   every other event from those states is illegal.
4. **B-004:** The valid and idempotent state/event pairs are exactly those in
   the Transition Contract below. Repetition may refresh compatible metadata
   only where the contract explicitly permits it.
5. **B-005:** A repeated PR- or task-binding event is idempotent only when it
   preserves the existing identity. A conflicting PR number, task identity,
   or merge-attempt identity is an error rather than a replacement.
6. **B-006:** Feedback recovery remains valid: a stale `claim:` placeholder
   may be reclaimed, and a placeholder may be replaced by its real feedback
   task. An unrelated active feedback task may not be replaced.
7. **B-007:** `Blocked` remains recoverable to `Done`, `Failed`, or
   `Cancelled`, including the existing externally merged PR reconciliation
   path. No event may reopen a terminal state.
8. **B-008:** Store mutations validate while holding the existing row lock.
   Rejection aborts the transaction, persists no partial field changes, and
   returns the error to the caller.
9. **B-009:** Rejection is never reported as success or downgraded to an
   ignored event. Store errors propagate to existing outer callers for logging
   or mapping according to their own contract; the Tier-C review fallback must
   not discard a failed `record_ready_to_merge_with_fallback` call or persist
   task completion first. The lifecycle write must succeed before the server
   records `TaskStatus::Done`, a `ready_to_merge` round, runtime feedback, or a
   completion event. Repeating merge approval from `Done` is an accepted
   idempotent event; merge approval from every other non-`ReadyToMerge` state is
   an error.
10. **B-010:** Existing serialized workflow rows remain readable without a
    migration, and valid existing paths retain their current wire states and
    metadata effects.
11. **B-011:** Concurrent valid and invalid updates remain serialized by the
    current store transaction. A rejected contender cannot overwrite the
    committed winner or persist a partially mutated snapshot.
12. **B-012:** The canonical workflow runtime and the SpecRail process
    contract remain behaviorally and structurally unchanged.

## Transition Contract

`same` means the event is accepted without changing the lifecycle state. Every
accepted event may replace `last_event`, advance `updated_at`, and replace
`last_remote_fact_hash` only when the event supplies a hash. Only the
event-specific fields named below may otherwise change; unlisted fields are
preserved. A compatible optional binding may fill an empty stored field, but a
different non-empty PR, task, or merge-attempt identity is illegal unless the
row explicitly authorizes a new stage binding.

| Event | Valid source state(s) | Result | Accepted metadata effect |
| --- | --- | --- | --- |
| `DependenciesDetected` | `Discovered`, `AwaitingDependencies` | `AwaitingDependencies` | Clear active task and review fallback. |
| `IssueScheduled` | `Discovered`, `AwaitingDependencies`, same-task `Scheduled` | `Scheduled` | Bind the scheduling task, replace `labels_snapshot` and `force_execute` with the supplied scheduling snapshot, and clear review fallback. |
| `ImplementStarted` | `Discovered`, `AwaitingDependencies`, same-task `Scheduled`, same-task `Implementing` | `Implementing` | Bind the implementation task; clear review fallback. |
| `ImplementStarted` | same-task `AddressingFeedback` | same | Retain feedback-stage state and task; clear review fallback. |
| `PlanIssueDetected` | same-task `Scheduled`, same-task `Implementing` | `Implementing` | Bind the same task, replace `plan_concern` with the supplied detail, and clear review fallback. |
| `PrDetected` | `Discovered`, same-task `Scheduled`, same-task `Implementing`, same-task `AddressingFeedback`, same-PR/task `PrOpen` | `PrOpen` | Bind the compatible PR number/URL/head and task; clear review fallback. `Discovered` preserves first-success recovery when PR evidence arrives before an earlier lifecycle write. |
| `FeedbackSweepCompleted` | `PrOpen`, `AwaitingFeedback` | `AwaitingFeedback` | Clear active task, feedback claim, and review fallback. |
| `FeedbackFound` | `PrOpen`, `AwaitingFeedback`, `FeedbackClaimed` | `FeedbackClaimed` | Clear active task, refresh claim time, and fill only compatible PR fields. |
| `FeedbackFound` | placeholder-backed `AddressingFeedback` | `FeedbackClaimed` | Reclaim the placeholder, clear active task, refresh claim time, and fill only compatible PR fields. |
| `FeedbackTaskScheduled` | `PrOpen`, `FeedbackClaimed` | `AddressingFeedback` | Bind the new feedback task, clear claim time and review fallback, and fill only compatible PR fields. |
| `FeedbackTaskScheduled` | placeholder-backed or same-task `AddressingFeedback` | same | Retain the same task or replace only the placeholder with the real task; clear claim time and review fallback. |
| `NoFeedbackFound` | `FeedbackClaimed`, `AwaitingFeedback` | `AwaitingFeedback` | Clear active task, feedback claim, and review fallback. |
| `Mergeable` | `PrOpen`, `AwaitingFeedback`, `AddressingFeedback`, `ReadyToMerge` | `ReadyToMerge` | Clear active task and feedback claim; fill only a missing or matching PR head. A supplied Tier-C fallback fills an empty `review_fallback`; when `tier`, `trigger`, and `active_bot` match an existing snapshot, preserve that complete first snapshot including `activated_at`; a different logical fallback is illegal. |
| `MergeStarted` | `ReadyToMerge`, same-task/head-attempt `Merging` | `Merging` | Bind the merge task and compatible head attempt; clear feedback claim. |
| `HumanMergeApproved` | `ReadyToMerge`, `Done` | `Done` | Clear feedback claim; a repeated `Done` event is audit-refresh only. |
| `WorkflowBlocked` | any nonterminal state | `Blocked` | Clear active task and feedback claim; preserve other bindings. |
| `WorkflowFailed` | any nonterminal state, `Failed` | `Failed` | Clear feedback claim; a repeated `Failed` event otherwise refreshes audit fields only. |
| `WorkflowCancelled` | any nonterminal state, `Cancelled` | `Cancelled` | Clear feedback claim; a repeated `Cancelled` event otherwise refreshes audit fields only. |
| `WorkflowDone` | any nonterminal state, `Done` | `Done` | Clear feedback claim and fill only missing or matching PR fields; a repeated `Done` event cannot replace bindings. |

For this contract, `Blocked` is nonterminal and recoverable. `Done`, `Failed`,
and `Cancelled` are terminal. All state/event pairs not represented above are
illegal.

## Acceptance Criteria

- [ ] A deterministic table test evaluates all 224 state/event combinations
      and proves each expected valid, idempotent, conditional, or illegal
      result.
- [ ] Illegal-transition tests compare the complete workflow snapshot before
      and after rejection.
- [ ] Terminal-state tests prove that late scheduling, implementation, PR,
      feedback, readiness, and merge-start events cannot reopen a workflow.
- [ ] Conditional tests cover matching and conflicting PR/task identities,
      placeholder reclaim, and placeholder-to-real-task binding.
- [ ] Accepted-event tests prove the metadata effects above, including
      audit-only terminal repetition and preservation of every unlisted field.
- [ ] Store tests prove scheduling metadata and the Tier-C review fallback
      snapshot are applied only after their lifecycle event validates.
- [ ] A server behavior test proves a rejected Tier-C lifecycle update leaves
      the task non-`Done`, appends no `ready_to_merge` round, emits no runtime
      ready-to-merge feedback, and records no completion event.
- [ ] A server retry test proves a lifecycle-success/task-store-failure retry
      preserves the first fallback snapshot and ultimately records exactly one
      `ready_to_merge` round, runtime feedback result, and completion event.
- [ ] Store tests prove rejected updates roll back without changing the
      persisted row.
- [ ] Tests retain `Blocked -> Done`, human approval, repeated terminal event,
      feedback-claim recovery, `Scheduled -> PlanIssueDetected`,
      first-success `Discovered -> PrDetected`, `Scheduled -> PrDetected`, and
      `PrOpen -> FeedbackTaskScheduled`.
- [ ] No lifecycle enum, wire tag, database schema, canonical runtime file, or
      SpecRail workflow file changes.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-002 and B-005. Transition validation occurs before optional event metadata can replace persisted fields; payload schema expansion is out of scope. |
| Error and failure paths | Covered by B-001, B-002, B-008, and B-009. |
| Authorization / permission | N/A. This change validates an internal domain transition and neither grants nor changes caller authority. |
| Concurrency / race / ordering | Covered by B-008 and B-011. |
| Retry / repetition / idempotency | Covered by B-003 through B-006. |
| Illegal state transitions | Covered by B-001 through B-007 and the complete Transition Contract. |
| Compatibility / migration | Covered by B-010 and B-012. |
| Degradation / fallback | Covered by B-009; rejection is an error, never success-shaped degradation. |
| Evidence and audit integrity | Covered by B-002, B-005, and B-008; rejected events cannot rewrite lifecycle evidence. |
| Cancellation / interruption / partial completion | Covered by B-003, B-007, B-008, and B-011. |

## Edge Cases

- A delayed `PrDetected` arrives after the workflow is `Done`, `Failed`, or
  `Cancelled`.
- A repeated `PrDetected` names a different PR from the durable binding.
- A feedback claim is retried after a crash before the real task ID is bound.
- A feedback-task event races with stale-claim reclamation.
- An externally merged PR is observed while the legacy workflow is `Blocked`.
- A merge-start event repeats with a different task or head identity.
- A store closure mutates metadata before discovering that its lifecycle event
  is illegal.

## Rollout Notes

No data migration or feature flag is required. Existing rows retain their
serialized representation. The change makes previously accepted invalid calls
return errors; callers and logs should therefore be monitored for transition
errors that reveal latent ordering defects. Reverting the implementation
restores permissive behavior but also restores terminal-state corruption risk.
