# Product Spec

## Linked Issue

GH-1770

## User Problem

Harness dispatches parallel agent workflows whose model spend is
unbounded by default. `max_budget_usd` exists but defaults to unlimited,
is set only by GC, and is enforced only by the Claude CLI rather than by
Harness. There is no aggregate ceiling: no per-workflow default, no
daily cap, and no admission-time check. Operators have already absorbed
the consequences — a queue-drain session that accumulated 944M tokens
and a 248-session retry storm — and each was stopped by a human, not by
the system.

Operators need spend to be bounded by server-enforced budgets with
graduated, visible responses, so that a single runaway workflow or an
aggregate dispatch storm degrades service predictably instead of
draining a quota window.

## Goals

- Give every workflow instance a USD budget by default, overridable per
  definition/profile via `WORKFLOW.md`, enforced server-side from
  streamed usage telemetry uniformly across all agent adapters.
- Add a per-profile daily spend cap with a graduated response ladder:
  throttle, then operator attention, then dispatch halt.
- Gate dispatch on remaining budget: the command dispatcher defers work
  with an explicit reason code when the workflow or daily budget is
  exhausted.
- Convert token telemetry to USD through a versioned pricing table, and
  label every dollar figure with an explicit confidence level
  (estimated vs provider-confirmed).

## Non-Goals

- Provider billing reconciliation; dollars remain estimates unless a
  provider-confirmed figure is available.
- Per-user or multi-tenant quota accounting.
- Changing GC's existing `budget_per_signal_usd` / `total_budget_usd`
  mechanics.
- Replacing or retiring the turn budget (`max_turns`); it remains an
  independent bound.
- Building new dashboard UI beyond exposing the budget/ledger fields to
  existing monitor endpoints.

## User-Visible Behavior

1. **B-001:** Every workflow instance created after rollout carries a
   USD budget: the profile default unless the workflow definition or
   submission supplies an override. A missing configuration yields the
   built-in default, never unlimited. An explicit opt-out to unlimited
   must be a deliberate configuration value, not an omission.
2. **B-002:** Accumulated workflow spend is computed server-side from
   adapter usage telemetry and persisted with the instance. Redispatch,
   retry, and recovery inherit the accumulated ledger; no path resets
   spend to zero for the same instance.
3. **B-003:** When a workflow's accumulated spend crosses its soft
   threshold, new activity dispatch for that workflow is deprioritized
   and the state is visible in runtime status. Crossing the hard
   ceiling blocks the workflow through the existing operator-attention
   path; the operator can raise the budget and unblock via existing
   recovery actions.
4. **B-004:** Per-profile daily spend is tracked against a configurable
   cap. Crossing the throttle threshold slows new dispatch for that
   profile; crossing the cap halts new dispatch for that profile while
   in-flight activities run to completion. The halt clears when the UTC
   day rolls over or the operator raises the cap.
5. **B-005:** The dispatcher evaluates remaining workflow and daily
   budget before dispatching a command. A denial is a deferred dispatch
   with a typed reason code, visible wherever existing dispatch-barrier
   reasons are visible. Denials are never silent drops.
6. **B-006:** Agent CLIs that accept a budget flag still receive
   `--max-budget-usd` derived from the remaining activity share.
   Server-side enforcement is authoritative: an adapter that ignores or
   cannot receive the flag is still terminated or not extended once the
   server-side ledger crosses the ceiling.
7. **B-007:** Every USD amount surfaced by API or logs carries a
   confidence label: `estimated` (derived via the pricing table) or
   `provider_confirmed`. An estimate is never presented as confirmed.
   Usage rows for models absent from the pricing table are priced with
   a conservative fallback and flagged.
8. **B-008:** Budget enforcement ships with a shadow mode: ledgers are
   computed and reason codes logged, but no dispatch is deferred and no
   workflow blocked. Enforcement activates per profile via
   configuration.
9. **B-009:** Budget state is observable: workflow status exposes
   budget, accumulated spend, and threshold state; profile status
   exposes daily spend, cap, and ladder position. Existing usage
   monitor endpoints gain the same fields rather than a parallel
   surface.
10. **B-010:** Exhaustion outcomes are distinct and typed. A workflow
    stopped for budget reasons is distinguishable from `max_turns`
    exhaustion, activity failure, and operator cancellation, in both
    runtime events and workflow data.

## Acceptance Criteria

- [ ] A workflow created with no budget configuration receives the
      built-in default; a definition override and a submission override
      each take precedence in documented order; tests cover all three.
- [ ] Ledger tests prove accumulated spend survives retry, redispatch,
      and recovery without reset.
- [ ] Threshold tests prove soft-threshold deprioritization, hard-cap
      operator-attention blocking, and operator unblock-with-raised-
      budget recovery.
- [ ] Daily-cap tests prove throttle → halt progression, in-flight
      completion during halt, and UTC-day reset.
- [ ] Dispatcher tests prove budget denial defers with the typed reason
      code and never silently drops a command.
- [ ] Adapter tests prove the CLI flag is derived from remaining share
      and that server-side termination triggers for an adapter that
      never receives a flag.
- [ ] Pricing tests prove token→USD conversion per token class, the
      conservative unknown-model fallback, and confidence labeling on
      every surfaced amount.
- [ ] Shadow-mode tests prove no enforcement side effects while
      shadow-mode ledgers and logs are produced.
- [ ] GC budget behavior is unchanged by the entire suite.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-001 and B-007: missing budget config yields defaults, never unlimited; unknown models price conservatively with a flag. |
| Error and failure paths | Covered by B-005, B-006, B-010: denials are typed and visible; adapter non-cooperation is bounded server-side; budget stops are distinct outcomes. |
| Authorization / permission | Raising budgets/caps is an operator configuration action via existing config and recovery surfaces; no new privilege model. |
| Concurrency / race / ordering | Covered by B-002 and B-004: ledgers are persisted with the instance and updated transactionally; daily counters tolerate concurrent workers (small overshoot from in-flight turns is accepted and bounded by B-004's in-flight rule). |
| Retry / repetition / idempotency | Covered by B-002: no ledger reset on any re-execution path. |
| Illegal state transitions | Budget blocking reuses existing operator-gate transitions; no new lifecycle states are introduced. |
| Compatibility / migration | Covered by B-008: shadow mode first; existing instances without ledgers begin accumulating from activation, never retroactively blocked. |
| Degradation / fallback | Covered by B-004 and B-007: halts are graduated and visible; pricing gaps degrade to conservative estimates, never silent zeros. |
| Evidence and audit integrity | Covered by B-009 and B-010: ledger values, threshold crossings, and stop reasons are persisted and queryable. |
| Cancellation / interruption / partial completion | Covered by B-004: in-flight activities complete during halts; partial turns still record their usage into the ledger. |

## Edge Cases

- An adapter reports usage late or never; the turn ends before
  telemetry arrives.
- A workflow's budget is lowered by configuration below its already
  accumulated spend.
- The pricing table lacks the model used by an in-flight turn.
- The UTC day rolls over while a profile is halted and jobs are queued.
- Candidate fan-out splits an activity share across candidates whose
  summed spend exceeds the parent share mid-flight.
- Shadow mode is switched to enforce while workflows are past their
  hard ceiling.
- Two workers commit usage for the same workflow concurrently.

## Rollout Notes

Phase 1 ships ledgers, pricing, and shadow mode; operators observe
computed spend and would-have-fired reason codes. Phase 2 activates
per-workflow enforcement for one low-volume profile, then broadly.
Phase 3 activates daily caps. Reverting enforcement returns to
observation-only behavior; ledgers and events remain. No database
migration is destructive; new columns/tables are additive.
