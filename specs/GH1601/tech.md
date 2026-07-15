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
| Unfenced status update | `crates/harness-workflow/src/runtime/store.rs:809-827` | `mark_command_status` clears owner and lease by command ID only. | Cannot provide B-002/B-003/B-008. |
| Atomic job enqueue | `crates/harness-workflow/src/runtime/store.rs:916-1025` | Locks the command, checks status and optional owner, dedupes an existing job, inserts a job, and marks the command dispatched in one transaction. | Model for fenced deferral and B-007 at-most-one enqueue. |
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
  "attempt": 2,
  "next_dispatch_at": "2026-07-15T12:00:00Z"
}
```

The Rust reason enum is a closed set with snake_case serialization:
`RuntimePolicyDisabled`, `WorkflowConfigInvalid`, and
`IsolationTierUnavailable`. Optional tier/trust fields are legal only for the
isolation reason. Constructors reject empty reason/project identity and any
reason/field mismatch (B-004, B-012).

Use a new migration to add the columns, replace the named status constraint
with the same vocabulary plus `deferred`, and replace the claim index with a
partial/covering index suitable for `(status, dispatch_not_before,
dispatch_lease_expires_at, created_at)`. Existing rows receive zero attempts,
NULL schedule, and NULL barrier; their existing status is unchanged (B-011).

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
    barrier_reason,
    now,
    backoff_policy,
) -> DeferClaimedCommandOutcome
```

The operation runs one transaction:

1. Lock the workflow instance and command in the repository's established lock
   order; load command status, owner, lease, attempt, and workflow terminal
   state.
2. If the workflow is terminal, apply the existing terminal-command disposition
   and return `WorkflowTerminal`; do not defer or create a job (B-009).
3. Require command status `dispatching`, matching `dispatch_owner`, and a claim
   still owned by that generation. Otherwise return `StaleClaim` without writes
   or events (B-003/B-008). Owner equality is the minimum fence; if the final
   implementation finds owner IDs reusable, add a persisted claim-generation
   token rather than relying on timestamps.
4. Compute the next attempt from persisted count using saturating exponential
   backoff with configured floor and finite ceiling. Update status to
   `deferred`, increment count, set `dispatch_not_before`, write the typed
   barrier, and clear owner/lease.
5. Insert `WorkflowRuntimeDispatchDeferred` through the existing
   transaction-scoped event helper with command/workflow/project IDs, reason
   code, explanation, attempt, next time, and isolation fields when applicable.
6. Commit once and return `Deferred`. Any error rolls back all fields and the
   event (B-002/B-013).

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
claim update sets `dispatching`, owner, and lease but preserves attempt/barrier
metadata until successful job enqueue. Keeping the last barrier during the
claim makes concurrent reads explain why the retry exists; successful enqueue
atomically clears schedule/barrier and leaves attempt count available for
historical projection, or records it in the dispatch event before clearing.

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
`enqueue_runtime_job_for_claimed_command` must verify terminal workflow state
under the same transaction/lock discipline as their command mutation. If the
terminal state wins, no job insert occurs (B-009).

Successful retry continues using the original `command.id` and
`(workflow_id, dedupe_key)` identity. Existing job lookup inside the enqueue
transaction remains the idempotency fence: a job found for the command yields
`AlreadyExists` and the command converges to `dispatched` (B-007/B-008).

### 6. Projection and observability

Runtime tree/summary and any command serialization expose:

- status `deferred`
- `dispatch_barrier.reason_code` and non-empty reason
- `dispatch_attempt_count`
- `dispatch_not_before`
- required isolation tier/trust class when applicable

Aggregate status maps include `deferred`. Invalid persisted barrier JSON returns
an error or an explicit invalid-evidence projection; it must not drop the row or
claim successful dispatch. `WorkflowRuntimeDispatchDeferred` is the append-only
audit source for each committed attempt (B-004/B-012).

## Data Flow

```text
pending/deferred-due command
  -> transactional claim (dispatching + owner + lease)
  -> load project policy/config and resolve required isolation
  -> barrier present
       -> fenced atomic defer transaction
          (deferred + attempt + due time + typed barrier + audit event)
       -> no job / no agent / workflow state unchanged
  -> later eligible claim
  -> barrier cleared
       -> fenced atomic job enqueue for original command
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
| B-003 live-owner fence | Store defer API | `cargo test -p harness-workflow defer_claimed_command_rejects_stale_owner` covers missing, wrong, repeated, and superseded owners with zero mutation/events. |
| B-004 typed complete reason evidence | Barrier reason type and event serialization | `cargo test -p harness-workflow dispatch_barrier_reason` table-tests all variants, rejects empty/mismatched fields, and round-trips event payloads. |
| B-005 no isolation downgrade | Workflow dispatcher | `cargo test -p harness-server runtime_command_dispatch_tick_defers_unavailable_isolation_without_fallback` asserts required container tier, available host tier, zero jobs/agent calls on initial and retry attempts. |
| B-006 bounded scheduled retry | Claim SQL and backoff calculator | `cargo test -p harness-workflow deferred_command_backoff` proves not-before exclusion, exact exponential steps, saturation at ceiling, and later eligibility. |
| B-007 original identity and one job | Claim plus atomic enqueue | `cargo test -p harness-workflow deferred_command_dispatches_original_command_once` retries the same ID/dedupe key concurrently and asserts one job. |
| B-008 competing dispatchers | Claim/defer/enqueue fencing | `cargo test -p harness-workflow deferred_command_dispatcher_race_has_one_winner` uses two owners and asserts one disposition/event or one job, never both from stale work. |
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
  cancellation, or completion. Reuse the established store lock order and add
  two-transaction race tests before implementation is accepted.
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

1. Land the migration, typed status/reason, store atomic API, and tests in one
   compatibility unit so no code path can write an unsupported status.
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
