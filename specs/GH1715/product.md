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
   ignored event inside the lifecycle/store boundary. Existing outer callers
   may log or map the returned error according to their own contract.
10. **B-010:** Existing serialized workflow rows remain readable without a
    migration, and valid existing paths retain their current wire states and
    metadata effects.
11. **B-011:** Concurrent valid and invalid updates remain serialized by the
    current store transaction. A rejected contender cannot overwrite the
    committed winner or persist a partially mutated snapshot.
12. **B-012:** The canonical workflow runtime and the SpecRail process
    contract remain behaviorally and structurally unchanged.

## Transition Contract

`same` means the event is accepted without changing the lifecycle state.
Identity conditions in B-005 and B-006 still apply.

| Event | Valid source state(s) | Result |
| --- | --- | --- |
| `DependenciesDetected` | `Discovered`, `AwaitingDependencies` | `AwaitingDependencies` |
| `IssueScheduled` | `Discovered`, `AwaitingDependencies`, `Scheduled` | `Scheduled` |
| `ImplementStarted` | `Discovered`, `AwaitingDependencies`, `Scheduled`, `Implementing` | `Implementing` |
| `ImplementStarted` | `AddressingFeedback` | same; retain feedback-stage ownership |
| `PlanIssueDetected` | `Implementing` | same |
| `PrDetected` | `Implementing`, `AddressingFeedback`, `PrOpen` | `PrOpen` |
| `FeedbackSweepCompleted` | `PrOpen`, `AwaitingFeedback` | `AwaitingFeedback` |
| `FeedbackFound` | `PrOpen`, `AwaitingFeedback`, `FeedbackClaimed` | `FeedbackClaimed` |
| `FeedbackFound` | placeholder-backed `AddressingFeedback` | `FeedbackClaimed` |
| `FeedbackTaskScheduled` | `FeedbackClaimed` | `AddressingFeedback` |
| `FeedbackTaskScheduled` | placeholder-backed or same-task `AddressingFeedback` | same |
| `NoFeedbackFound` | `FeedbackClaimed`, `AwaitingFeedback` | `AwaitingFeedback` |
| `Mergeable` | `PrOpen`, `AwaitingFeedback`, `AddressingFeedback`, `ReadyToMerge` | `ReadyToMerge` |
| `MergeStarted` | `ReadyToMerge`, same-attempt `Merging` | `Merging` |
| `HumanMergeApproved` | `ReadyToMerge`, `Done` | `Done` |
| `WorkflowBlocked` | any nonterminal state, `Blocked` | `Blocked` |
| `WorkflowFailed` | any nonterminal state, `Blocked`, `Failed` | `Failed` |
| `WorkflowCancelled` | any nonterminal state, `Blocked`, `Cancelled` | `Cancelled` |
| `WorkflowDone` | any nonterminal state, `Blocked`, `Done` | `Done` |

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
- [ ] Store tests prove rejected updates roll back without changing the
      persisted row.
- [ ] Tests retain `Blocked -> Done`, human approval, repeated terminal event,
      and feedback-claim recovery behavior.
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
