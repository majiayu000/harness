# GH1607 Product Spec: External-State Continuation Loop for prompt_task Workflows

GitHub issue: `#1607`

## Goals

- Let a single submitted prompt task drive an external unit of work (GitHub
  issue, Linear ticket, any tracker subject) across multiple agent turns:
  the runtime re-dispatches the implement activity with attempt context
  while the agent reports the external subject as still active.
- Keep the harness process free of tracker calls: the agent is the probe
  and reports observed external state as structured activity output; the
  runtime only decides.
- Bound every loop: attempt budget, no-progress detection, and operator
  cancellation all terminate the loop in an auditable state.
- Preserve today's single-shot prompt_task behavior exactly when no
  continuation policy is supplied.

## Non-Goals

- No polling of GitHub/Linear/any tracker from the harness server process.
- No changes to `github_issue_pr`, `pr_feedback`, or `quality_gate`
  workflow definitions or their reducers.
- No new runtime kind, dispatch semantics, or lease changes.
- Not a general declarative-definition mechanism (GH-1609 tracks that).

## Users

- Operators running Symphony-style unattended tracker loops who today must
  resubmit a prompt task manually after every agent turn.
- WORKFLOW.md authors who write status-map prompt bodies and need the
  runtime to keep invoking the agent until the tracker subject settles.

## Behavior Invariants

- `B-001` A prompt submission without a continuation policy behaves
  exactly as today: one implement activity, success transitions to
  `done`, and no new fields are required anywhere in the submission path.
- `B-002` A continuation policy is validated at submission time:
  `max_attempts >= 1` and bounded by a server-side cap (default 20),
  `attempt_delay_secs` bounded (default cap 3600), and a non-empty
  `active_states` set. Invalid policies reject the submission with an
  actionable error; there is no silent clamping.
- `B-003` The harness process never contacts the external tracker. The
  loop decision consumes only the `external_state` `ActivitySignal`
  reported by the agent on its `ActivityResult`.
- `B-004` On implement success with a continuation policy, an
  `external_state` signal matching `active_states`, and attempts
  remaining, the workflow re-enters `implementing` and the next implement
  activity is enqueued in the same completion transaction with attempt
  `N+1` context — never a driverless active state.
- `B-005` On implement success with an `external_state` signal outside
  `active_states`, the workflow transitions to `done` through the existing
  evidence and MarkDone path, unchanged.
- `B-006` With a continuation policy configured, a missing, unparsable, or
  ambiguous `external_state` signal transitions the workflow to `blocked`
  with an auditable reason. It never silently completes as `done` and
  never silently retries.
- `B-007` Attempt budget exhaustion (`attempt > max_attempts` would be
  required to continue) transitions to `blocked` with a reason carrying
  the last observed external state; it never reports success.
- `B-008` No-progress guard: an attempt counts as no-progress when its
  reported `external_state` value is identical to the previous attempt's
  AND its `ActivityResult` carries no new artifacts and no new validation
  records (both mechanically checkable on the result; summary text
  changes alone do not count as progress). `no_progress_limit` (default
  3) consecutive no-progress attempts transition to `blocked` for
  operator attention instead of consuming the remaining attempt budget.
- `B-009` The attempt counter, last observed external state, and the
  policy itself are persisted on the workflow instance and survive server
  restart; a restarted server resumes the loop with the same bounds.
- `B-010` Every attempt is an ordinary runtime job: leases, retry policy,
  circuit breaker, and per-attempt audit records apply without exception.
  A failed (not blocked) attempt consumes the existing activity retry
  budget, not the continuation attempt budget.
- `B-011` The agent prompt for attempt `N+1` carries the attempt number,
  the previous attempt's reported external state, and the previous attempt
  summary, so the agent can resume instead of restarting.
- `B-012` Operator cancellation between attempts terminates the loop in
  `cancelled`; no further attempts are enqueued after a cancel is
  observed.
- `B-013` Every loop decision (continue, finish, block on malformed
  signal, block on exhaustion, block on no-progress) is recorded as a
  workflow decision with the triggering signal as evidence.

## Boundary Checklist

| Category | Coverage |
|---|---|
| Empty input | covered: B-001 (absent policy = today's behavior), B-002 (empty `active_states` rejected) |
| Failure paths | covered: B-006 (malformed signal), B-010 (failed attempts use activity retry budget) |
| Authorization | N/A — no new endpoints; submission uses the existing authenticated path; B-003 removes any new external credential surface |
| Concurrency | covered: B-004 (same-transaction re-enqueue, no driverless state), B-010 (leases per attempt) |
| Retry + idempotency | covered: B-004 (attempt-scoped dedupe keys), B-009 (restart-safe counters), B-010 (retry budgets separated) |
| Illegal transitions | covered: B-006/B-007 (no silent done), B-012 (no enqueue after cancel) |
| Compatibility | covered: B-001 (byte-identical single-shot path), B-005 (existing done path reused) |
| Degradation / fallback | covered: B-006 (blocked, never silent fallback), B-008 (no-progress → operator attention) |
| Evidence / audit | covered: B-013 (decision records per loop verdict), B-011 (attempt context traceable) |
| Cancellation / partial | covered: B-012 (cancel between attempts), B-007 (exhaustion leaves auditable blocked state) |

Cross-product boundary called out explicitly: a restart between attempt
`N` completing and attempt `N+1` being dispatched must resume from
persisted state without double-enqueueing (B-004 dedupe + B-009), and an
agent that stops reporting the signal mid-loop must land in `blocked`,
not `done` (B-006 + B-013).
