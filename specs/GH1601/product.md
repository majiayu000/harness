# Product Spec

## Linked Issue

GH-1601

Complexity: large. This change adds a durable command state, retry scheduling,
concurrency fencing, persistence migration, and operator-visible evidence across
the workflow-runtime and server dispatch boundaries.

## User Problem

A workflow can have exactly one command that represents its next required
activity. If project runtime dispatch or workers are disabled, `WORKFLOW.md` is
malformed, or the required isolation tier is unavailable, the dispatcher
currently ends that command without creating a runtime job. The workflow itself
remains in an active state, but no live command remains to advance it. Restoring
the project configuration or isolation dependency does not recover the work.

This is especially misleading for operators: the runtime appears to have an
active workflow, while the only continuation is terminal and invisible to the
claim loop. Re-submission or direct database repair may be required even though
no agent execution began and no business decision failed.

## Goals

1. Preserve a durable continuation whenever a claimed runtime command encounters
   a pre-enqueue dispatch barrier.
2. Retry that same logical command after a bounded delay and dispatch it once the
   barrier clears, without creating duplicate runtime jobs.
3. Commit the deferred disposition and its audit evidence atomically and only
   for the current dispatch owner.
4. Keep required isolation fail closed: unavailability delays execution but
   never selects a weaker tier.
5. Make deferred work and its reason visible enough for operators to distinguish
   intentional waiting from an orphaned active workflow.
6. Preserve existing behavior for commands that do not hit a dispatch barrier,
   commands belonging to terminal workflows, and already-persisted status rows.

## Non-Goals

- No generic transition of every workflow definition to `blocked` or `failed`.
- No change to activity execution, result reduction, or workflow transitions
  after a runtime job has been created.
- No weakening, fallback, or automatic installation of an isolation tier.
- No new operator recovery endpoint and no expansion of GH-1567 recovery to
  non-issue workflow definitions.
- No general workflow-liveness watchdog; this spec addresses the known
  pre-enqueue dispatch barriers only.
- No retry of commands already terminal for reasons outside this dispatch path.

## User-Visible Behavior

When runtime policy, project workflow configuration, or required isolation
prevents job creation, the command remains a live but deferred continuation.
The workflow stays in its existing active state because no business transition
has occurred. Runtime status identifies the barrier, the last attempt, and the
next eligible dispatch time. Once the barrier clears and the delay expires, the
same command is retried and may create one runtime job.

An operator who intentionally disables a project sees the queued work as
deferred, not as completed, skipped, or failed. A malformed configuration is
reported as a configuration barrier rather than silently using defaults. An
unavailable isolation tier is reported with the required tier and trust class,
and execution never falls back to weaker isolation.

## Testable Invariants

- B-001: If a claimed runtime command cannot create a job because project
  runtime dispatch or workers are disabled, `WORKFLOW.md` cannot be loaded or
  parsed, or its required isolation tier is unavailable, the workflow retains
  exactly one durable live continuation for that logical command; the command
  is not recorded as `skipped`, `failed`, `completed`, or successfully
  dispatched.
- B-002: The deferred command status, next eligible dispatch time, attempt
  count, typed barrier reason, dispatch-lease release, and workflow audit event
  are committed atomically. A reader can observe either the complete prior
  claim or the complete deferred disposition, never a mixture.
- B-003: Only the current dispatch owner of a `dispatching` command may defer
  it. A missing owner, stale owner, expired claim that has been reclaimed, or
  repeated disposition from the same owner performs no mutation and emits no
  audit event.
- B-004: Barrier reasons use a closed, typed vocabulary containing at least
  `runtime_policy_disabled`, `workflow_config_invalid`, and
  `isolation_tier_unavailable`. Evidence includes the command and workflow IDs,
  project identity, attempt, next eligible time, and a non-empty explanation;
  isolation evidence also includes required tier and trust class.
- B-005: Required isolation is fail closed. An unavailable required tier
  creates no runtime job, invokes no agent, and never substitutes a weaker
  available tier, including on every retry.
- B-006: Deferred retries use persisted, bounded backoff. A command is not
  claimable before its next eligible time; repeated barriers increase delay up
  to a finite ceiling, preventing a hot loop while guaranteeing periodic
  reevaluation.
- B-007: After the barrier clears and the command becomes eligible, the same
  command ID and dedupe key are used for dispatch. Across retries it creates at
  most one runtime job, and a successful enqueue clears deferred scheduling and
  barrier metadata.
- B-008: With concurrent dispatcher replicas, at most one owner may commit a
  deferred disposition or enqueue a job for a claim generation. A losing or
  stale dispatcher cannot overwrite a newer disposition, shorten its backoff,
  or append duplicate evidence.
- B-009: If the owning workflow becomes terminal before deferred disposition or
  retry enqueue commits, the terminal state wins: the workflow is never
  reopened, no new runtime job is created, and the command follows the existing
  terminal-workflow cancellation/skip contract.
- B-010: Deferred status, reason, attempt count, and next eligible time survive
  process restart. Restart neither makes the command terminal nor resets its
  backoff to an immediate retry.
- B-011: Existing database rows and API consumers remain compatible. All
  pre-existing command status values retain their meaning; rows missing the new
  scheduling and reason fields are interpreted according to their existing
  status and are not fabricated as deferred.
- B-012: Operator projections and aggregate command-status counts identify
  deferred work as deferred and expose its typed reason and next eligible time.
  Missing, empty, or invalid barrier evidence must be surfaced as invalid
  evidence; it must not be displayed as successful dispatch or silently
  replaced by an invented value.
- B-013: If persistence fails or the process is interrupted before the atomic
  disposition commits, no partial deferred event or metadata remains. The
  existing dispatch claim remains recoverable through lease expiry; the failed
  attempt does not consume a retry or create a runtime job.
- B-014: Intentional runtime disablement is waiting, not success. The deferred
  command remains eligible for bounded reevaluation, does not increment agent
  dispatch/success counters, and dispatches only after both dispatch and worker
  policy are enabled.

## Boundary Checklist

| Category | Verdict |
| --- | --- |
| Empty / missing input | covered: B-004 requires complete typed evidence; B-012 fails visibly on missing, empty, or invalid evidence rather than inventing data. |
| Error and failure paths | covered: B-001 names all three barriers; B-005 covers isolation failure; B-013 covers store failure and interruption. |
| Authorization / permission | covered: B-003 treats the dispatch owner and live claim as the authority to mutate; B-005 preserves isolation policy authority. No new external endpoint is added. |
| Concurrency / race / ordering | covered: B-002 defines atomic visibility; B-008 covers competing dispatchers; B-009 covers terminal-workflow races. |
| Retry / repetition / idempotency | covered: B-003 rejects repeated disposition; B-006 bounds retries; B-007 preserves identity and at-most-one job; B-010 persists retry state. |
| Illegal state transitions | covered: B-001 preserves the active business state and live continuation; B-009 forbids reopening a terminal workflow. |
| Compatibility / migration | covered: B-011 preserves old status meanings and defines missing-field behavior. |
| Degradation / fallback | covered: B-005 forbids isolation downgrade; B-014 distinguishes intentional waiting from success; B-012 forbids silent evidence fallback. |
| Evidence and audit integrity | covered: B-002 makes evidence atomic with disposition; B-004 defines required evidence; B-008 prevents duplicate/stale evidence. |
| Cancellation / interruption / partial completion | covered: B-009 preserves terminal cancellation semantics; B-013 defines crash/store-failure recovery with no partial commit. |

## Acceptance Criteria

- [ ] All three named dispatch barriers satisfy B-001 through B-006 and create
  no runtime job while the barrier remains.
- [ ] Clearing each barrier satisfies B-007 and B-014 with the original command
  identity and at most one runtime job.
- [ ] Concurrent, stale-owner, terminal-state, and interrupted-transaction tests
  prove B-002, B-003, B-008, B-009, and B-013.
- [ ] A restart-style persistence test proves B-010 without relying on an
  in-memory timer.
- [ ] Migration and projection tests prove B-011 and B-012 for both new and
  legacy rows.
- [ ] Required-isolation negative tests prove B-005 on initial dispatch and
  retry.

## Edge Cases

- A project is disabled after a command is claimed but before its policy is
  checked: the current owner records one deferred disposition (B-002, B-003).
- The project is enabled before the retry time: the persisted delay is honored;
  the command is not hot-looped or specially awakened (B-006).
- `WORKFLOW.md` alternates between malformed and disabled across attempts: the
  latest typed reason is visible, the attempt count remains monotonic, and the
  command identity is unchanged (B-004, B-007).
- Isolation availability changes between resolution and enqueue: enqueue must
  still use the resolved required tier; no weaker tier may be selected (B-005).
- An old dispatcher finishes after a lease expires and another dispatcher
  claims the command: the old owner cannot defer or enqueue it (B-003, B-008).
- Cancellation occurs while a command is deferred or being reclaimed: the
  workflow terminal state wins and no agent starts (B-009).
- The event insert fails after the command update is attempted: the transaction
  rolls back both writes and lease expiry remains the recovery path (B-013).

## Rollout Notes

The new status and persistence fields require a forward database migration and
updates to every exhaustive command-status match. Deploy migration-capable code
before relying on deferred rows. Rollback is safe only while no `deferred` rows
exist, or after an explicit rollback migration converts them to `pending` while
preserving their original command identity. Operators should be told that
disabled projects will retain visible queued work and periodically reevaluate
policy instead of discarding commands.
