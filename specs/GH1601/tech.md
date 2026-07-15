# Tech Spec

## Linked Issue

GH-1601

## Product Spec

`specs/GH1601/product.md`

## Codebase Context (verified at `edc63a13`)

| Area | Verified anchor | Current behavior | Relevance |
| --- | --- | --- | --- |
| Project policy dispatch | `crates/harness-server/src/http/background/runtime_command_dispatch.rs:91-166` | Disabled dispatch/worker marks the claimed command `skipped`; malformed `WORKFLOW.md` marks it `failed` and appends a separate config-error event. | Primary B-001/B-002 failure paths. |
| Project policy selection | `crates/harness-server/src/http/background/runtime_command_dispatch.rs:262-309` | Loads per-project workflow config and returns `None` when either runtime loop is disabled. | Supplies the `runtime_policy_disabled` barrier. |
| Isolation dispatch | `crates/harness-workflow/src/runtime/dispatcher.rs:199-312` | Required-tier unavailability marks the command `failed`, then appends a separate isolation event; successful dispatch calls the claimed-command enqueue API. | Must become fail-closed deferral without weakening tier selection. |
| Command claim | `crates/harness-workflow/src/runtime/store.rs:741-806` | Claims only `pending` and expired `dispatching` commands using `FOR UPDATE ... SKIP LOCKED`; no scheduled deferred state exists. | Extend eligibility with persisted due time and retain concurrent claiming. |
| Reused dispatcher identity | `crates/harness-workflow/src/runtime/dispatcher.rs:112-194` | One `RuntimeCommandDispatcher` stores one `dispatcher_id` and reuses it across `dispatch_once` and every command in `dispatch_pending`. | Owner equality cannot distinguish an expired attempt from a later claim by the same dispatcher; every claim needs a persisted generation fence. |
| Unfenced status update | `crates/harness-workflow/src/runtime/store.rs:809-827` | `mark_command_status` clears owner and lease by command ID only. | Cannot provide B-002/B-003/B-008. |
| Atomic job enqueue | `crates/harness-workflow/src/runtime/store.rs:916-1025` | Locks the command and checks status plus optional owner, but a reused owner can satisfy a later claim; the existing-job replay path also does not identify the generation that inserted the job. | Require owner plus generation for claimed enqueue and define generation-aware replay before relying on B-007 at-most-one enqueue. |
| Event append | `crates/harness-workflow/src/runtime/store.rs:350-385` | Allocates workflow event sequence and commits in its own transaction. | Deferral must use transaction-scoped event insertion instead of this standalone API. |
| Status type | `crates/harness-workflow/src/runtime/status.rs:4-60` | Typed command vocabulary has no `Deferred` variant. | Additive Rust and wire-state change; exhaustive matches must be audited. |
| Command schema | `crates/harness-workflow/src/runtime/store_migrations.rs:51-62,135-143` | Commands have status and dispatch lease columns but no defer scheduling or reason columns. | Migration surface for B-006/B-010. |
| Status constraint | `crates/harness-workflow/src/runtime/store_migrations.rs:188-204` | PostgreSQL check constraint allows the existing nine statuses only. | Must accept `deferred` without weakening other constraints. |
| Existing integration tests | `crates/harness-server/src/http/tests/runtime_dispatch_tests.rs:94-251,633-690` | Tests assert no job plus terminal command status for isolation/config/disabled barriers, but not continued liveness or recovery. | Convert to product-invariant regression tests. |

## Proposed Design

### 1. Durable command representation

Add `WorkflowCommandStatus::Deferred` with wire value `deferred`. Extend the
persisted command record and table with:

- `dispatch_not_before TIMESTAMPTZ NULL`
- `dispatch_attempt_count BIGINT NOT NULL DEFAULT 0`
- `dispatch_claim_generation BIGINT NOT NULL DEFAULT 0`
- `dispatch_barrier JSONB NULL`

`dispatch_barrier` is a typed serialized object, not an open-ended replacement
for status:

```json
{
  "reason_code": "isolation_tier_unavailable",
  "reason": "required container isolation is unavailable: docker missing",
  "project_id": "/repo",
  "required_tier": "container",
  "trust_class": "non_collaborator",
  "dispatch_owner": "dispatcher:2f3f...",
  "claim_generation": 3,
  "attempt": 2,
  "next_dispatch_at": "2026-07-15T12:00:00Z"
}
```

The Rust reason enum is a closed set with snake_case serialization:
`RuntimePolicyDisabled`, `WorkflowConfigInvalid`, and
`IsolationTierUnavailable`. Optional tier/trust fields are legal only for the
isolation reason. Constructors reject empty reason/project identity and any
reason/field mismatch (B-004, B-012).

Use a new migration to add the columns, constrain `dispatch_claim_generation`
to a non-negative PostgreSQL `BIGINT`, replace the named status constraint with
the same vocabulary plus `deferred`, and replace the claim index with a
partial/covering index suitable for `(status, dispatch_not_before,
dispatch_lease_expires_at, created_at)`. Existing rows receive zero attempts
and generation, NULL schedule, and NULL barrier; their existing status is
unchanged (B-011). Each successful claim increments generation in the same SQL
statement that sets `dispatching`, owner, and lease. Overflow fails the claim
transaction closed; generation is never reset by deferral, enqueue, restart,
or legacy-row migration.

`WorkflowCommandRecord` exposes the typed optional barrier and scheduling
fields. `Deferred` is an active continuation in all active-command predicates,
cancel/supersede logic, runtime-tree aggregation, replay suppression, and PR
feedback active-command checks. It is not treated as completed or failed.

### 2. Atomic fenced deferral

Add a store operation conceptually shaped as:

```rust
defer_claimed_command_if_owned(
    command_id,
    dispatch_owner,
    dispatch_claim_generation,
    barrier_reason,
    now,
    backoff_policy,
) -> DeferClaimedCommandOutcome
```

The operation runs one transaction:

1. Lock the workflow instance and command in the repository's established lock
   order; load command status, owner, lease, attempt, and workflow terminal
   state.
2. If status is `deferred`, the command generation equals the supplied
   generation, and the persisted barrier identifies the same owner and
   generation, return `AlreadyDeferred` without writes. This is the only
   successful deferral replay case.
3. Otherwise require command status `dispatching`, matching `dispatch_owner`, and exact
   `dispatch_claim_generation`. Generation is an unconditional fence because
   `RuntimeCommandDispatcher` reuses its dispatcher ID. Owner equality alone is
   forbidden, and lease timestamps are not substitutes for generation.
   Otherwise return `StaleClaim` without writes or events (B-003/B-008).
4. If the workflow is terminal, apply the existing terminal-command disposition
   and return `WorkflowTerminal`; do not defer or create a job (B-009).
5. Compute the next attempt from persisted count using saturating exponential
   backoff with configured floor and finite ceiling. Update status to
   `deferred`, increment count, set `dispatch_not_before`, write the typed
   barrier, and clear owner/lease.
6. Store the dispatch owner and claim generation in the barrier and insert
   `WorkflowRuntimeDispatchDeferred` through the existing
   transaction-scoped event helper with command/workflow/project IDs, reason
   code, explanation, dispatch owner, claim generation, attempt, next time, and
   isolation fields when applicable.
7. Commit once and return `Deferred`. Any error rolls back all fields and the
   event (B-002/B-013).

An exact replay of a committed deferral for the same command and generation
returns `AlreadyDeferred` without changing backoff, barrier data, or audit
events. A replay after a later claim observes a different generation and
returns `StaleClaim`, even when the owner string is identical. This makes
retries after a lost response idempotent without allowing an old attempt to
overwrite a newer one.

No workflow business-state transition occurs. The invariant is that an active
workflow retains a live deferred command, not that infrastructure policy should
be encoded as a definition-specific `blocked` transition. This also avoids
expanding the GH-1567 recovery API, which currently targets issue workflows.

### 3. Scheduled claiming and backoff

Extend `claim_pending_commands` so candidates are:

- `pending`; or
- `deferred` with `dispatch_not_before <= CURRENT_TIMESTAMP`; or
- `dispatching` with an expired lease.

All candidates remain protected by `FOR UPDATE OF command SKIP LOCKED`. The
claim update atomically increments `dispatch_claim_generation` and returns that
value with the command while setting `dispatching`, owner, and lease. It
preserves attempt/barrier metadata until successful job enqueue. Keeping the
last barrier during the claim makes concurrent reads explain why the retry
exists; successful enqueue atomically clears schedule/barrier and leaves
attempt count and generation available for replay and historical projection.

Backoff uses repository workflow-runtime dispatch configuration rather than a
new global service. Add validated positive floor/ceiling fields with conservative
defaults. Calculation uses saturating arithmetic and `min(ceiling, floor *
2^(attempt-1))`. There is no jitter in the initial contract so deterministic
tests and operator next-time evidence agree exactly. Multiple commands remain
spread by their original creation order; jitter can be a separately specified
scalability change if production evidence requires it.

### 4. Barrier classification at dispatch boundaries

Replace the three terminal status paths:

- `Ok(None)` from project policy selection ->
  `runtime_policy_disabled` and atomic deferral.
- `RuntimeDispatchProfileSelectionError::WorkflowConfig` ->
  `workflow_config_invalid` and atomic deferral.
- `ensure_tier_available` error -> `isolation_tier_unavailable` and atomic
  deferral carrying the already-resolved required tier/trust class.

All return a new `CommandDispatchOutcome::Deferred` rather than overloading
`Skipped`. Tick summaries count `deferred` separately. Agent dispatch/success
counters are unchanged because no agent call occurred (B-014).

Other errors keep their current lease-expiry retry behavior unless the
implementation can classify them into one of the three specified barriers.
They must not be silently coerced into `workflow_config_invalid`; this keeps the
scope and evidence vocabulary honest.

Isolation resolution is rerun on every eligible attempt. Each attempt must
again prove the required tier available before enqueue, so a prior or concurrent
availability observation never authorizes a weaker tier (B-005).

### 5. Terminal races and job identity

The current pre-dispatch terminal check and later standalone status update have
a race. Both the atomic deferral operation and
`enqueue_runtime_job_for_claimed_command` must accept the claim generation and
verify owner plus exact generation and terminal workflow state under the same
transaction/lock discipline as their command mutation. If the fence does not
match, the operation returns `StaleClaim` before any job, command, or event
write. If the terminal state wins, no job insert occurs (B-008/B-009).

Successful retry continues using the original `command.id` and
`(workflow_id, dedupe_key)` identity. The enqueue transaction records the
dispatch owner and claim generation with the runtime job evidence before
clearing owner/lease and keeps the generation on the command. After locking the
workflow and command it first recognizes an exact committed replay only when
status is `dispatched`, the command generation equals the supplied generation,
and the existing job evidence names the same owner and generation; it then
returns `AlreadyExists` without a second insert or command update. Otherwise it
requires current `dispatching` status plus the supplied owner and generation
before checking terminal state or mutating. A job or command from a different
generation does not authenticate the caller: the old caller receives
`StaleClaim` and performs no mutation. Fence validation, job lookup, job insert,
and the `dispatched` command update share one transaction (B-007/B-008).

### 6. Projection and observability

Runtime tree/summary and any command serialization expose:

- status `deferred`
- `dispatch_barrier.reason_code` and non-empty reason
- `dispatch_attempt_count`
- `dispatch_claim_generation`
- `dispatch_not_before`
- required isolation tier/trust class when applicable

Aggregate status maps include `deferred`. Invalid persisted barrier JSON returns
an error or an explicit invalid-evidence projection; it must not drop the row or
claim successful dispatch. `WorkflowRuntimeDispatchDeferred` is the append-only
audit source for each committed attempt (B-004/B-012).

## Data Flow

```text
pending/deferred-due command
  -> transactional claim (dispatching + owner + incremented generation + lease)
  -> load project policy/config and resolve required isolation
  -> barrier present
       -> owner+generation-fenced atomic defer transaction
          (deferred + attempt + due time + typed barrier + audit event)
       -> no job / no agent / workflow state unchanged
  -> later eligible claim
  -> barrier cleared
       -> owner+generation-fenced atomic job enqueue for original command
          (one runtime job + command dispatched + defer metadata cleared)
       -> existing worker/reducer flow
```

On a transaction or process failure before defer commit, no defer event or
metadata survives; the `dispatching` claim becomes reclaimable after its lease
expires. On restart after commit, eligibility derives only from PostgreSQL
status and timestamps (B-010/B-013).

## Alternatives Considered

- **Transition every workflow to `blocked`: rejected.** It mixes infrastructure
  readiness with business state, requires definition-specific transitions, and
  leaves non-issue child workflows without the existing operator recovery path.
- **Mark command `pending` immediately: rejected.** Without persisted scheduling
  it hot-loops, loses the barrier distinction, and cannot expose a stable next
  retry time.
- **Keep `failed`/`skipped` and add a sweeper: rejected.** It preserves the
  contradictory terminal-command/active-workflow state and introduces a second
  recovery owner.
- **Create a runtime job that immediately fails: rejected.** No agent activity
  is eligible to run, it misstates execution evidence, and it routes a dispatch
  barrier through post-job reducer semantics.
- **Fall back to host isolation: rejected.** Violates B-005 and the repository's
  fail-closed isolation contract.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Executable verification |
| --- | --- | --- |
| B-001 durable continuation for all three barriers | Server dispatch classification plus workflow dispatcher | `cargo test -p harness-server runtime_command_dispatch_tick_defers_` runs disabled-policy, malformed-config, and unavailable-isolation DB cases; each asserts active workflow, one deferred command, zero jobs. |
| B-002 atomic disposition and evidence | `WorkflowRuntimeStore::defer_claimed_command_if_owned` | `cargo test -p harness-workflow defer_claimed_command_is_atomic` forces event insertion failure and asserts command, lease, attempt, schedule, and event log all remain pre-transaction. |
| B-003 live-owner fence | Store defer API | `cargo test -p harness-workflow defer_claimed_command_rejects_stale_owner` covers missing/wrong owners, exact-generation replay, and the same owner reused after generation advances; stale cases produce zero mutation/events and exact replay produces no additional mutation/event. |
| B-004 typed complete reason evidence | Barrier reason type and event serialization | `cargo test -p harness-workflow dispatch_barrier_reason` table-tests all variants, rejects empty/mismatched fields, and round-trips event payloads. |
| B-005 no isolation downgrade | Workflow dispatcher | `cargo test -p harness-server runtime_command_dispatch_tick_defers_unavailable_isolation_without_fallback` asserts required container tier, available host tier, zero jobs/agent calls on initial and retry attempts. |
| B-006 bounded scheduled retry | Claim SQL and backoff calculator | `cargo test -p harness-workflow deferred_command_backoff` proves not-before exclusion, exact exponential steps, saturation at ceiling, and later eligibility. |
| B-007 original identity and one job | Claim plus atomic enqueue | `cargo test -p harness-workflow deferred_command_dispatches_original_command_once` retries the same ID/dedupe key and exact generation, asserts one job, and verifies exact replay returns `AlreadyExists` without another write. |
| B-008 competing dispatchers | Claim/defer/enqueue fencing | `cargo test -p harness-workflow deferred_command_dispatcher_race_has_one_winner` covers different owners and the same reused owner across two generations; it asserts one winning disposition/event or job, `StaleClaim` for the old generation, and no stale mutation. |
| B-009 terminal workflow wins | Store deferral and enqueue terminal guard | `cargo test -p harness-workflow terminal_workflow_wins_dispatch_race` terminalizes between claim and mutation, then asserts no reopen/job and existing terminal command disposition. |
| B-010 restart durability | PostgreSQL command fields and due-time claim | `cargo test -p harness-workflow deferred_command_survives_store_restart` drops the store handle, reconnects, and asserts preserved status/reason/attempt/time and no early claim. |
| B-011 legacy compatibility | Migration, status parsing, exhaustive matches | `cargo test -p harness-workflow runtime_store_migrates_deferred_commands` seeds all old statuses before migration and asserts unchanged meanings plus NULL new fields; `cargo check --workspace --all-targets` catches missed matches. |
| B-012 projection and invalid evidence | Runtime tree/summary serialization | `cargo test -p harness-server runtime_tree_reports_deferred_command` covers valid fields/count; companion invalid-JSON fixture must return explicit degraded/error evidence, not success. |
| B-013 rollback/interruption recovery | Atomic defer transaction and lease reclaim | `cargo test -p harness-workflow failed_defer_reclaims_after_lease_expiry` proves rollback leaves no event/attempt and a later owner can reclaim after expiry. |
| B-014 disabled means waiting, not success | Server tick outcome and metrics | `cargo test -p harness-server runtime_command_dispatch_tick_defers_disabled_policy_without_agent_metrics` asserts `Deferred`, deferred tick count, no skipped/failed/success or agent-dispatch count, then dispatch after re-enable. |

## Risks

- **Security:** A regression could weaken isolation during retry. Mitigate with
  B-005 negative tests on both first attempt and retry and by carrying the
  resolved required tier in evidence, never as authorization.
- **Correctness:** Adding a status changes many exhaustive active/terminal
  predicates. Grep every `WorkflowCommandStatus` match and command-status SQL
  list; compile with `-D warnings` and exercise cancel, replay, PR feedback, and
  retention paths.
- **Concurrency:** Inconsistent lock order could deadlock with enqueue,
  cancellation, or completion. Owner-only fencing would also admit an old
  attempt after the same dispatcher ID is reused. Reuse the established store
  lock order, require generation on both claimed mutations, and add
  two-transaction same-owner/different-generation race tests before
  implementation is accepted.
- **Compatibility:** Old binaries cannot parse `deferred` and the old DB check
  constraint rejects it. Require migration and binary rollout together; do not
  write deferred rows until migration succeeds.
- **Performance:** Due-command scans add another predicate/index. Verify
  PostgreSQL query plans for large deferred populations and keep batch limits.
- **Operations:** Permanently disabled projects retain deferred work. Bounded
  backoff and explicit projections prevent churn and make intentional waiting
  visible.

## Test Plan

- [ ] Store unit/integration tests named in B-002, B-003, B-006 through B-011,
  and B-013 run against PostgreSQL.
- [ ] Claim-generation tests prove every claim atomically increments the
  persisted generation, overflow rolls back, same-generation replays are
  idempotent, and a reused owner with an older generation cannot defer or
  enqueue.
- [ ] Server dispatch tests named in B-001, B-005, B-012, and B-014 cover the
  full project-policy path.
- [ ] `cargo test -p harness-workflow command_dispatcher`
- [ ] `cargo test -p harness-server runtime_command_dispatch`
- [ ] `cargo check --workspace --all-targets`
- [ ] Before commit: `cargo fmt --all` and `cargo fmt --all -- --check`.
- [ ] Before PR push: `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Manual database check: inspect one deferred row and its matching event,
  re-enable the project, wait until due, and confirm the original command owns
  exactly one runtime job. This supplements but does not replace automated
  tests.

## Rollout Plan

1. Stop dispatch claimers, let current claims drain or expire, then land the
   migration, typed status/reason, persisted claim generation, store atomic API,
   and tests in one compatibility unit. Pre-migration `dispatching` rows at
   generation zero must be reclaimed by the new code before mutation; no
   owner-only compatibility path is allowed.
2. Switch the three dispatcher barriers to the new API and expose projection
   fields.
3. Deploy through the normal migration-capable server startup and monitor
   deferred counts, defer attempt rate, and oldest deferred age.
4. Treat a growing oldest age as an operator configuration/dependency signal,
   not as successful throughput.

## Rollback Plan

Before rollback, stop new dispatch claims. If no deferred rows exist, revert the
code while leaving additive nullable columns and the expanded constraint in
place. If deferred rows exist, run an explicit compatibility migration that
converts them to `pending`, clears defer schedule/barrier fields, and preserves
command ID/dedupe key; then deploy the old binary. Never map deferred rows to
`skipped` or `failed`, because that recreates GH-1601. Reverting the dispatcher
classification alone is not an acceptable rollback while deferred rows remain.
