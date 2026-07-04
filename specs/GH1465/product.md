# Product Spec

## Linked Issue

GH-1465

## User Problem

Operators cannot tell stuck tasks from in-flight tasks. Execution records
(2026-04-29..30, `harness-codex-runtime` data dir) show 21 created tasks
producing 86 `reviewing` + 86 `waiting` transitions, hitting the turn cap of
10, and then stopping silently: zero `completed`, zero `failed`. A task that
exhausts its review-round budget looks identical to one that is still
working, so stuck work is only discoverable by hand-diffing event logs.
GH-1438 shows the same "silently stuck, no terminal signal" family is still
alive on the current runtime via a different mechanism.

## Goals

- Guarantee a liveness invariant: every created task emits exactly one
  terminal event (`completed`, `failed`, or `stalled`).
- Make budget exhaustion an explicit, operator-visible terminal outcome with
  a machine-readable reason.
- Enforce the invariant with a regression eval so future runtime refactors
  cannot silently reintroduce limbo states.

## Non-Goals

- Changing review-loop quality behavior (what counts as pass/fail per round).
- Reworking the reconciler's illegal-transition handling (tracked in GH-1438).
- Retroactively repairing historical April-era event logs.

## Behavior Invariants

1. Every task that emits `created` eventually emits exactly one terminal
   event; no code path may abandon a task in `waiting`, `reviewing`, or any
   other non-terminal status.
2. When the round/turn budget is exhausted, the task transitions to an
   explicit terminal state (`stalled` or `failed`) carrying reason
   `round_budget_exhausted`, the rounds consumed, and the last status.
3. The terminal event is emitted at error level to the operator surface
   (events log + status API), never as a silent stop (U-29: no silent
   degradation).
4. Status surfaces (dashboard, status API, intake) count budget-exhausted
   tasks as terminal, not in-flight.
5. The invariant holds under crash/restart: recovery either resumes the task
   or drives it to a terminal state — never back into indefinite limbo.

## Acceptance Criteria

- [ ] A task driven to its turn cap in a test emits a terminal event with
      reason `round_budget_exhausted`.
- [ ] An event-log audit check (test or eval) fails when any `created` task
      lacks a terminal event after the run drains.
- [ ] Status API reflects the terminal state; no task remains `waiting`
      after budget exhaustion.
- [ ] Restart mid-review resumes or terminates the task; a soak/recovery
      test asserts no limbo survivors.

## Edge Cases

- Budget exhausted while an external review (bot/human) is genuinely
  pending — terminal `stalled` must record what it was waiting on so the
  operator can requeue.
- Concurrent terminal proposals (budget exhaustion racing task completion) —
  exactly-one-terminal-event must hold; first writer wins, second is a no-op.
- Tasks created but never scheduled (queue drained/shutdown) — shutdown path
  must also terminalize or persist-and-resume, not drop.

## Rollout Notes

Pure runtime-behavior fix plus an eval gate; no config surface. Announce the
new `stalled` reason string in CHANGELOG so downstream log consumers can key
on it.
