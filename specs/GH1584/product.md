# Product Spec

## Linked Issue

GH-1584

## User Problem

GH-1567 gave operators an escape hatch (`POST /api/workflows/runtime/unblock`
and `/retry`) for `blocked` and `failed` workflow-runtime instances, with
structured stop reasons. Recovery is intentionally conservative: every stopped
workflow waits for a human, even when the block reason is transient and
machine-recheckable — a GitHub rate limit that expires, a CI backend outage
that heals, merge-base drift that a rebase already resolved, or a runtime
circuit-breaker cooldown that has elapsed. Operators end up performing rote
recheck-and-unblock actions that a bounded, audited policy could perform for
them, while genuinely terminal blocks (maintainer approval needed, fatal or
configuration failures) must keep waiting for a human forever.

## Goals

1. Classify structured stop reasons into `transient` (auto-recheckable) and
   `terminal` (operator-only), extending the GH-1567 reason taxonomy.
2. Provide a per-repo, default-OFF auto-recovery policy: transient reasons get
   bounded rechecks (max attempts, exponential backoff, jitter); a successful
   recheck re-runs exactly the same eligibility path as a manual unblock/retry.
3. Record every automatic recheck and recovery as an audit event (reason
   class, attempt count, outcome) before the recovery takes effect.
4. When attempts are exhausted, escalate exactly like today: the workflow
   stays stopped for the operator monitor, and an escalation event is emitted
   for alerting once GH-1582 lands.
5. Terminal reasons never auto-recover, regardless of configuration.

## Non-Goals

- No change to the manual unblock/retry API contract shipped by GH-1567.
- No auto-recovery for `cancelled` workflows.
- No force-retry of non-retryable `error_kind` values (`fatal`,
  `configuration`).
- No alerting transport in this issue — escalation emits events that GH-1582
  consumes.
- No global default-ON mode; policy is opt-in per repository.

## Testable Invariants

- B-001: Every structured stop reason maps to exactly one reason class,
  `transient` or `terminal`, via a single classification function; the same
  classifier is used by the scheduler, the API serialization, and the audit
  events.
- B-002: A stop reason that is missing, empty, unknown, or unclassifiable is
  classified `terminal` (fail closed). It is never auto-rechecked, and the
  classification decision itself is visible in the API payload.
- B-003: With auto-recovery disabled (global default, or repo not opted in),
  no automatic recheck, unblock, or retry ever occurs; runtime behavior is
  byte-identical to GH-1567 semantics.
- B-004: With auto-recovery enabled for a repo, only workflows whose active
  stop reason classifies as `transient` are ever scheduled for recheck.
- B-005: A successful automatic recheck transitions the instance through the
  same recovery function as the manual operator action (same target state
  `planning`, same plan-activity re-enqueue, same `last_stop` evidence
  preservation, same active-stop-field clearing), differing only in the
  recorded actor/source and reason text.
- B-006: The number of automatic recovery attempts per stopped instance never
  exceeds the configured `max_attempts`; the attempt counter is persisted in
  the workflow instance data and survives server restart.
- B-007: Recheck scheduling uses exponential backoff with jitter; the next
  eligible recheck time is persisted, so a server restart neither resets the
  backoff to zero nor causes an immediate recheck burst.
- B-008: For every automatic recheck attempt, an audit event containing the
  reason class, attempt number, and outcome is appended to the workflow event
  log BEFORE the state transition is committed. There is no reachable state
  in which an instance was auto-recovered but no audit event exists.
- B-009: An automatic recheck that loses a race with a concurrent manual
  operator unblock/retry (or any other state change) observes the changed
  state, records the attempt outcome as `superseded`, performs no transition,
  and does not consume the instance as double-recovered. Exactly one recovery
  transition is committed per stop episode.
- B-010: When attempts are exhausted, the instance remains in its stopped
  state, appears in the operator monitor exactly as an unrecovered GH-1567
  instance does, and a single `AutoRecoveryExhausted` escalation event is
  emitted (consumable by GH-1582 alerting). Exhaustion never deletes or masks
  stop evidence.
- B-011: A fresh stop episode (the instance re-enters `blocked` or `failed`
  after a successful recovery) resets the attempt counter; attempts are
  counted per stop episode, not per instance lifetime.
- B-012: Terminal-classified reasons are never auto-recovered even if a
  configuration file, environment override, or API request attempts to force
  it; the only recovery path for terminal reasons is the manual operator API.
- B-013: Manual unblock/retry remains available and unchanged for both reason
  classes at all times, including while a recheck is pending or backoff is in
  progress.
- B-014: Instances created before this feature (no reason-class field, legacy
  free-text reasons) are treated as `terminal` and never auto-recovered; the
  API tolerates the missing fields without fabricating a class.
- B-015: A recheck whose transient condition still holds (recheck fails)
  records the failed attempt with its evidence, increments the counter, and
  reschedules with increased backoff; it performs no state transition.
- B-016: Config validation rejects nonsensical policy values (zero or negative
  `max_attempts` when enabled, backoff ceiling below floor, jitter ratio
  outside [0, 1]) at startup rather than at recheck time.

## Boundary Checklist

| Category | Coverage |
| --- | --- |
| Empty input | covered: B-002 (missing/empty reason fails closed), B-014 (legacy rows without class) |
| Failure paths | covered: B-015 (failed recheck), B-010 (exhaustion escalates, evidence preserved) |
| Authorization | N/A + reason: the scheduler is an in-process trusted actor; the only externally callable surfaces remain the GH-1567 routes behind the existing API auth middleware, whose contract is unchanged (B-003, B-013). No new public endpoint is added. |
| Concurrency | covered: B-009 (recheck vs manual operator race, single recovery per episode) |
| Retry / idempotency | covered: B-006 (bounded attempts, persisted counter), B-007 (backoff persisted), B-011 (per-episode reset) |
| Illegal transitions | covered: B-012 (terminal never auto-recovers), B-004 (only transient scheduled), B-005 (only the established recovery transition is used) |
| Compatibility | covered: B-003 (default OFF is byte-identical to GH-1567), B-014 (legacy rows tolerated) |
| Degradation / fallback | covered: B-002 (unknown class degrades to terminal, visibly, never silently to transient), B-016 (invalid config fails startup, no silent clamping) |
| Evidence / audit | covered: B-008 (audit before effect), B-001 (class visible everywhere), B-010 (exhaustion event) |
| Cancellation / partial | covered: B-009 (superseded attempt commits nothing partial); cancelled workflows are out of scope by Non-Goals and remain terminal-by-state |

## Edge Cases

- The workflow runtime store is unavailable when a recheck fires: the attempt
  is skipped and rescheduled, not counted as consumed, and logged at error
  level.
- The server restarts mid-backoff: persisted `next_attempt_at` and attempt
  counter resume the schedule (B-006, B-007).
- The repo opts out of auto-recovery while attempts are pending: pending
  rechecks for that repo stop firing; no partial state is left behind.
- Two harness replicas or overlapping ticks pick up the same instance: the
  state-guarded recovery commit ensures one winner (B-009).
- The related GitHub issue is closed during backoff: the recheck re-runs the
  standard eligibility path, which produces a fresh structured blocker rather
  than dispatching work against a closed issue.

## Rollout Notes

Ship in small PRs: (1) reason classification + persistence of class on stop
metadata; (2) policy config + validation; (3) recheck scheduler + audit
events; (4) operator monitor / runtime tree exposure and docs. The feature is
inert until a repo sets the opt-in flag, so each PR is safe to land
independently. Rollback at any stage returns behavior to GH-1567 semantics
because classification fields are additive JSON.
