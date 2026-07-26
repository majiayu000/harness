# Runtime Reliability Analysis: Leases, Versioning, Retention, and Loop Supervision

> Status: analysis reference for GH-1772
> Date: 2026-07-26
> Method: read-only source inspection on `main` (81c78255); all file:line
> citations verified against that revision.

## Scope

This report analyzes four reliability defect clusters in the workflow runtime
and server orchestration layer:

1. Two independent lease systems (runtime jobs vs workspaces) that can
   disagree, with completed work silently dropped when they do.
2. `WorkflowInstance.version` — a serialized counter shaped like an optimistic
   concurrency token that no code path actually checks.
3. Retention and watchdog loops that exist but default off, leaving unbounded
   table growth and unsurfaced stuck workflows as the default posture.
4. A single-process orchestrator whose background loops are unsupervised and
   whose per-tick config reload turns a malformed `WORKFLOW.md` into a silent
   partial outage.

Each cluster is presented as evidence → failure scenario → remediation
direction. The corresponding fail-closed behavior contract lives in
`specs/GH1772/`.

## Cluster A — Dual leases: lost completions and dirty re-runs

### Evidence

Two lease systems govern one unit of work, in two different tables, with two
different liveness models, and no atomic relationship:

- **Runtime job leases** — `runtime_jobs` rows carry
  `WorkflowLease { owner, expires_at }` plus a `lease_generation` counter.
  Claim, defer, and completion are owner-scoped:
  `claim_next_runtime_job_excluding_runtime_kind`,
  `defer_runtime_job_claim_if_owned`, and
  `commit_runtime_activity_completion_with_transcript_if_owned`
  (`crates/harness-workflow/src/runtime/store/runtime_job_leases.rs`,
  `runtime/lease_state.rs`, `runtime/job_claim.rs`). TTL is
  `runtime_worker.lease_ttl_secs: 600` (`WORKFLOW.md:53`, applied at
  `crates/harness-server/src/http/background/runtime_workers.rs:6-7`).
- **Workspace leases** — `workspace_leases` rows keyed
  `(store_key, project_key, slot_index)` carry `owner_session`, `process_id`,
  and `process_started_at`, with liveness judged by PID inspection via
  `sysinfo` (`crates/harness-server/src/workspace_lease_store.rs:19-77`).

The worker renews its job lease during execution
(`execute_with_lease_renewal`, `crates/harness-workflow/src/runtime/worker.rs:162`),
but when renewal lapses — a long blocking turn, a paused process, a renewal
write failure — the completion path fails *silently from the workflow's
perspective*:

```text
crates/harness-workflow/src/runtime/worker.rs:172-188
    let Some(completion) = self.store
        .commit_runtime_activity_completion_with_transcript_if_owned(...)
        .await?
    else {
        tracing::warn!(
            runtime_job_id = %job.id,
            owner = %self.owner,
            "runtime job completion ignored because the worker no longer owns the lease"
        );
        return Ok(None);
    };
```

The entire `ActivityResult` — potentially a finished implementation turn with
a pushed branch — is dropped with a `warn`. No runtime event records the
rejected completion; no dead-letter row preserves the result; nothing tells
the workspace lease system that its co-owner just lost the job.

### Failure scenario

1. Worker W1 claims job J (lease TTL 600s) and acquires workspace slot S.
2. The agent turn runs 12 minutes; lease renewal misses the TTL window.
3. Worker W2 claims J (lease expired) and — because slot S is still leased to
   W1's session — acquires a *different* slot, or waits, while W1's agent
   continues mutating S.
4. W1's agent finishes; `...if_owned` returns `None`; the result (including
   the transcript that `prepare_runtime_transcript` just built) is discarded.
5. W2 re-runs the same activity from scratch. If it lands on slot S after
   W1's PID finally exits, it inherits a dirty tree with W1's uncommitted or
   unpushed state; if W1's agent had already pushed, W2's fresh attempt now
   races a remote branch that the workflow believes does not exist.

Every step is individually "working as designed"; the composition loses work,
occupies workspaces, and can produce duplicate PRs or corrupted branch state.

### Remediation direction

- **Single ownership epoch.** Introduce an ownership epoch (the existing
  `lease_generation` is the natural candidate) minted at job claim and
  propagated into the workspace lease row. Workspace lease acquisition and
  renewal validate the epoch; job lease renewal extends both records in one
  transaction where they share a store, or via an epoch check where they do
  not.
- **Dead-letter, not drop.** A completion rejected for lost ownership must be
  persisted as a dead-letter completion (job id, owner, epoch, full
  `ActivityResult`, transcript reference) and surfaced as a runtime event, so
  reconciliation can adopt the result when the re-run has not yet started, or
  an operator can compare divergent attempts when it has.
- **Cross-system release.** Losing the job lease must schedule release of the
  workspace lease held under the same epoch once the agent process exits,
  instead of leaving slot occupancy to PID-liveness decay.

## Cluster B — `WorkflowInstance.version`: a CAS token nothing checks

### Evidence

`WorkflowInstance` carries `pub version: u64`
(`crates/harness-workflow/src/runtime/model.rs:87`). It is incremented on
mutation (`instance.version = instance.version.saturating_add(1)`,
`crates/harness-workflow/src/runtime/store/instances.rs:80,124`) and bound
into every INSERT/UPDATE (`instances.rs:48,226`,
`store/transaction_helpers.rs:170,202`).

It is **never read back as a predicate**. Every mutation path serializes on a
pessimistic row lock plus a state-string check:

```text
crates/harness-workflow/src/runtime/store/transaction_helpers.rs:93
    sqlx::query_as("SELECT data::text FROM workflow_instances WHERE id = $1 FOR UPDATE")
```

with `WorkflowDecisionTransition { expected_state, .. }` supplying the
precondition. A grep for `version` in a `WHERE` clause across
`runtime/store/` returns nothing.

### Why this matters

Correctness currently depends on an unwritten rule: *every* writer must go
through `select_instance_for_update_tx` inside one transaction. The field's
name and type advertise the opposite contract — read, modify, write back with
`WHERE version = $expected`. The first future code path that takes the
advertised contract at face value (an API-side patch endpoint, a bulk
migration, a reconciliation fix-up) will read version N, write version N+1
without a lock, and silently overwrite a concurrent committed transition.
Because the column is dead weight today, no test can catch that regression:
the decoy is strictly worse than either honest alternative.

### Remediation direction

Decide once, mechanically:

- **Enforce**: make every instance UPDATE carry
  `WHERE id = $1 AND version = $expected` and treat zero rows affected as a
  typed conflict, keeping the row lock for the multi-statement transactions
  that need it; or
- **Remove/rename**: drop the field from the public model (or rename it to
  `mutation_count` with a doc comment stating it is informational), so no
  future writer can mistake it for a guard.

Either resolution is acceptable; the current ambiguous state is not.

## Cluster C — Retention and watchdog default off

### Evidence

`WORKFLOW.md` ships, and documentation recommends, the following defaults:

```text
WORKFLOW.md:61   orphan_reaper_enabled: true
WORKFLOW.md:65   workflow_watchdog_enabled: false
WORKFLOW.md:69   runtime_retention_enabled: false
WORKFLOW.md:73   task_retention_enabled: false
```

Consequences of the default posture:

- `workflow_events`, `workflow_decisions`, `runtime_events`, and
  `workflow_artifacts` — the last including full agent transcripts stored via
  the durable-transcript path — grow without bound.
- Stuck workflows in `blocked` / `awaiting_feedback` are never surfaced,
  because the loop that would alert on them (`http/workflow_watchdog.rs`) is
  compiled, spawned, and then no-ops every tick behind its config gate.
- `postgres_catalog.rs` only counts schemas and warns past a threshold; the
  one reaper that acts (`orphan_reaper_enabled: true`) covers only
  path-derived schemas.

The codebase has already paid for this exact defect class once. The orphan
reaper's own module comment is a postmortem:

```text
crates/harness-server/src/http/orphan_reaper.rs:1-14
//! `reap_orphaned_path_schemas` was added in #1216 but only exposed via the
//! `harness-pg-schema-cleanup` CLI — it was never wired into the running
//! server, so Postgres catalog growth was never actually bounded
//! automatically (a declaration-execution gap).
```

That gap produced a Postgres catalog that peaked around 538k schemas before
the reaper landed. Retention today sits in the same pre-reaper position:
implemented, spawned, and off.

### Remediation direction

- Ship safe-by-default retention: terminal-workflow event/decision compaction
  and transcript archival after a conservative age (e.g. 30 days), applied
  only to workflows in a terminal state.
- First activation runs in an operator-visible dry-run mode: the retention
  tick reports what *would* be deleted (counts, oldest/newest, bytes) as
  runtime events for at least one interval before destructive mode engages.
- Watchdog enabled by default with alert-on-transition semantics (already
  implemented at `workflow_watchdog.rs:17-24`) so enabling it is not noisy.

## Cluster D — Unsupervised loops and the silent config outage

### Evidence

The HTTP server entry point spawns the orchestration loops directly:

```text
crates/harness-server/src/http/mod.rs:190-217
    background::spawn_auto_recovery(&state);
    background::spawn_runtime_pr_feedback_sweeper(&state);
    orphan_reaper::spawn_orphan_schema_reaper(&state);
    workflow_watchdog::spawn_workflow_watchdog(&state);
    runtime_retention::spawn_runtime_retention(&state);
    background::spawn_runtime_command_dispatcher(&state);
    background::spawn_runtime_job_workers(&state);
    ...
```

Each helper is a bare `tokio::spawn` holding a `Weak<AppState>` in a
`loop { work; sleep(interval) }` (e.g. `background/auto_recovery.rs:111`,
`background/pr_feedback.rs:258`, `background/runtime_workers.rs:164`,
`background/runtime_command_dispatch.rs:494`). Properties of this shape:

- **No supervision.** A panicked or wedged loop dies (or stalls) invisibly;
  nothing restarts it and no health surface reports it absent. Failure
  handling inside the loops is warn-and-continue.
- **No cross-instance coordination.** The dispatcher identity is
  per-process-random (`dispatcher_id: format!("dispatcher:{}", Uuid::new_v4())`,
  `runtime/dispatcher.rs:141`). Safety under two server processes on one
  schema rests entirely on every individual claim being lease-guarded; the
  loops themselves have no leader election or singleton guard.
- **Per-tick config reload with divergent failure arms.** Watchdog, retention,
  and reaper re-run `load_workflow_config` on every tick. On parse failure the
  watchdog logs a warn (`workflow_watchdog.rs:28-37`); the orphan reaper's
  failure arm is `Err(_) => { sleep(30s); continue; }` with **no log at all**
  (`orphan_reaper.rs:40-48`). A malformed `WORKFLOW.md` therefore disables
  watchdog, retention, and reaping simultaneously — the reaper silently —
  while the dispatcher and job workers keep executing agent work at full
  speed. The system's *safety* loops fail off while its *spend* loops fail on.

### Remediation direction

- **Supervised task set.** Register each background loop in a supervisor
  (name, tick interval, last-success timestamp). The health endpoint reports
  per-loop staleness; a loop that has not completed a tick within N×interval
  flips the health surface to degraded.
- **Loud degraded mode on config failure.** A `WORKFLOW.md` parse failure is
  one event affecting every consumer; it must be surfaced once, at error
  level, on the health endpoint, and — because safety loops are disabled
  while dispatch continues — should optionally gate new dispatch after a
  configurable grace period. All config-reload failure arms converge on one
  shared handler so no loop can silently diverge again.
- **Leader-election design note.** Multi-instance orchestration is out of
  scope for the first remediation, but the supervisor abstraction should keep
  singleton loops (retention, reaper, watchdog) behind an advisory-lock
  acquisition so a future second instance is safe by construction rather than
  by audit.

## Priority and sequencing

| Order | Item | Rationale |
| --- | --- | --- |
| 1 | Dead-letter for rejected completions (A) | Stops silent loss of finished agent work; smallest blast radius. |
| 2 | Ownership epoch binding job + workspace leases (A) | Removes the dirty re-run window; builds on the dead-letter path for adoption. |
| 3 | Config-failure loud mode + loop supervision (D) | Cheap; converts silent partial outages into visible degraded state. |
| 4 | Retention defaults with dry-run (C) | Unbounded growth is certain, just slow; dry-run makes activation safe. |
| 5 | `version` enforce-or-remove (B) | No live defect today; closes a latent trap before new writers appear. |

## Related work

- `specs/GH1772/product.md`, `specs/GH1772/tech.md` — the behavior contract
  derived from this analysis.
- `docs/postgres-schema-cleanup.md`, `crates/harness-server/src/http/orphan_reaper.rs`
  — prior instance of the declaration-execution-gap class.
- GH-1716 (recovery CAS conflict handling) — adjacent spec covering recovery
  action races; Cluster B here is the instance-mutation counterpart.
