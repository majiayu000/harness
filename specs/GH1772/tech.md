# Tech Spec

## Linked Issue

GH-1772

## Current State

All citations verified on `main` (81c78255).

### Dual lease systems

- Runtime job leases: `runtime_jobs` rows carry `WorkflowLease { owner,
  expires_at }` and `lease_generation`; owner-scoped operations are
  `claim_next_runtime_job`
  (`crates/harness-workflow/src/runtime/store/runtime_jobs.rs:251`),
  `defer_runtime_job_claim_if_owned` (`store/runtime_job_state.rs:48`), and
  `commit_runtime_activity_completion_with_transcript_if_owned`
  (`store/activity_completion.rs:72`). TTL from
  `WORKFLOW.md:53` (`lease_ttl_secs: 600`) applied at
  `crates/harness-server/src/http/background/runtime_workers.rs:6-7`.
- Workspace leases: `workspace_leases` keyed `(store_key, project_key,
  slot_index)` with `owner_session`, `process_id`, `process_started_at`, PID
  liveness via `sysinfo`
  (`crates/harness-server/src/workspace_lease_store.rs:19-77`).
- Rejected completion path: `crates/harness-workflow/src/runtime/worker.rs:172-188`
  — `commit_runtime_activity_completion_with_transcript_if_owned` returning
  `None` produces `tracing::warn!` + `return Ok(None)`; the `ActivityResult`
  and prepared transcript are discarded. Lease renewal exists
  (`execute_with_lease_renewal`, `worker.rs:162`) but the drop path remains
  whenever renewal lapses.
- No field or transaction ties `lease_generation` to a workspace lease row.

### Version decoy

- `WorkflowInstance.version: u64` (`runtime/model.rs:87`), incremented at
  `runtime/store/instances.rs:80,124` and bound on writes
  (`instances.rs:48,226`, `store/transaction_helpers.rs:170,202`), but never
  used in a WHERE predicate. Mutation serializes on
  `SELECT ... FOR UPDATE` + `expected_state`
  (`store/transaction_helpers.rs:93`).

### Retention / watchdog defaults

- `WORKFLOW.md:61-73`: `orphan_reaper_enabled: true`,
  `workflow_watchdog_enabled: false`, `runtime_retention_enabled: false`,
  `task_retention_enabled: false`.
- `crates/harness-server/src/http/orphan_reaper.rs:1-14` documents the prior
  declaration-execution gap for schema reaping.

### Loop supervision

- Spawn sites: `crates/harness-server/src/http/mod.rs:190-217` (auto
  recovery, pr-feedback sweeper, pr hygiene, orphan reaper, watchdog,
  retention, command dispatcher, job workers, scheduler, alerting), each a
  bare `tokio::spawn` loop over a `Weak<AppState>`.
- Config reload per tick with divergent failure arms: watchdog warns
  (`http/workflow_watchdog.rs:28-37`); orphan reaper's arm is
  `Err(_) => { sleep(30s); continue; }` with no log
  (`http/orphan_reaper.rs:40-48`); retention mirrors the watchdog
  (`http/runtime_retention.rs:16-27`).
- Dispatcher identity is per-process-random
  (`runtime/dispatcher.rs:141`).

## Design

### D1 — Ownership epoch and dead-letter completions

Additive schema:

```sql
ALTER TABLE workspace_leases ADD COLUMN ownership_epoch BIGINT;  -- nullable

CREATE TABLE runtime_dead_letter_completions (
    id             TEXT PRIMARY KEY,
    runtime_job_id TEXT NOT NULL,
    owner          TEXT NOT NULL,
    ownership_epoch BIGINT NOT NULL,
    result         JSONB NOT NULL,
    transcript_ref TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    adopted_at     TIMESTAMPTZ,
    adopted_by     TEXT,
    superseded_by_rerun BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE UNIQUE INDEX ON runtime_dead_letter_completions (runtime_job_id, ownership_epoch);
```

- The epoch is the job's existing `lease_generation` at claim time; no new
  counter. `claim_next_runtime_job_*` already increments it; the claim result
  exposes it to the worker, which passes it to workspace acquisition.
- `workspace_lease_store` acquisition/renewal accepts an optional epoch; when
  present, renewal with an epoch older than the stored one is rejected
  (return value, not error), and release-by-epoch marks the row releasable
  once PID liveness fails.
- Worker completion path: on `...if_owned` returning `None`, insert the
  dead-letter row inside a new store call
  `dead_letter_runtime_completion(...)`, emit runtime event
  `ActivityCompletionDeadLettered`, then return. The unique index makes crash
  retry idempotent.
- Adoption: the auto-recovery sweep (`http/background/auto_recovery.rs`)
  gains a step — for each unadopted, non-superseded dead letter whose job has
  not been re-claimed since the epoch (`lease_generation` unchanged), commit
  the stored result through the normal completion path within one
  transaction, stamping `adopted_at`/`adopted_by`. If `lease_generation`
  advanced, stamp `superseded_by_rerun = TRUE` and leave it for operators
  (surfaced via the existing runtime events feed).

### D2 — Version resolution

Recommended resolution: **enforce**. Instance UPDATE statements gain
`AND version = $expected`; helper `update_instance_checked_tx` returns a
typed `InstanceVersionConflict` error on zero rows. The row lock remains for
multi-statement transactions; the predicate is defense-in-depth against
writers that bypass the helper. If review instead selects removal, the field
is renamed `mutation_count` with a doc comment and a test asserting no SQL
references `version` as a predicate. Exactly one resolution ships (product
B-005).

### D3 — Retention defaults and dry-run

- Config default flips: `runtime_retention_enabled: true`,
  `task_retention_enabled: true`, new `retention_dry_run: true` (per store,
  auto-cleared after the first completed dry-run interval is acknowledged via
  the existing config file, or left permanently on by the operator).
- Eligibility: instance in a terminal state (`Succeeded`/`Failed`/
  `Cancelled` mapping already defined by the state registry) AND
  `updated_at < now() - retention_age` (default 30 days). Only
  event/decision/artifact/transcript rows belonging to eligible instances are
  candidates. Non-terminal instances are structurally excluded by the query.
- Dry-run emits runtime event `RetentionDryRunReport { would_delete_events,
  would_delete_decisions, would_delete_artifacts, estimated_bytes,
  oldest, newest }` per tick.

### D4 — Loop supervisor and loud config failure

- New `BackgroundLoopSupervisor` in `harness-server`: `register(name,
  interval)` returns a `LoopTicket`; loops call `ticket.tick_ok()` /
  `ticket.tick_failed(reason)`. Supervisor state feeds the existing health
  route: per-loop `{ name, last_ok, staleness, degraded }`, degraded when
  `now - last_ok > staleness_factor * interval` (default factor 3).
- Shared config loader: one `load_workflow_config_tracked(state)` wrapper
  used by every config-gated loop. It logs at error level on the
  Ok→Err transition only, sets a shared `config_degraded_since` in
  `AppState`, and clears it on recovery. Individual loops lose their private
  failure arms (fixing the reaper's silent arm).
- Dispatch pause: the command dispatcher and job-worker claim path check
  `config_degraded_since`; beyond `config_failure_grace_secs` (default 300)
  they defer claims with reason code `config_degraded` (reusing the existing
  dispatch-barrier defer mechanism). Running activities are unaffected.
- Leader-election forward-compatibility note (**non-normative**, per product
  Non-Goals): a future multi-instance deployment could have the supervisor
  take `singleton: bool` at registration and acquire a Postgres advisory lock
  keyed by loop name before each singleton tick, making a second process safe
  by construction. This spec neither requires nor tests that behavior; it
  only asks that the supervisor API not preclude it.

## Affected Files (expected)

- `crates/harness-workflow/src/runtime/worker.rs` — dead-letter branch.
- `crates/harness-workflow/src/runtime/store/{runtime_jobs.rs,
  runtime_job_state.rs, activity_completion.rs}`,
  `store/transaction_helpers.rs`, `store/instances.rs`,
  `store_migrations.rs` — epoch exposure, dead-letter table, version
  predicate.
- `crates/harness-server/src/workspace_lease_store.rs`,
  `workspace_pool.rs` — epoch-aware acquisition/renewal/release.
- `crates/harness-server/src/http/background/auto_recovery.rs` — adoption
  sweep.
- `crates/harness-server/src/http/{mod.rs,workflow_watchdog.rs,
  runtime_retention.rs,orphan_reaper.rs}` and
  `http/background/*.rs` — supervisor registration, shared config loader.
- `crates/harness-core/src/config/workflow.rs` — new defaults and knobs.
- `WORKFLOW.md` — default changes and documentation.

## Validation

- `cargo test -p harness-workflow` (store, worker, reducer suites; requires
  `HARNESS_DATABASE_URL` for the Postgres-backed lease/dead-letter tests).
- `cargo test -p harness-server` (workspace lease, background loop,
  health-route suites).
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --all -- --check` before push.

## Risks

- Epoch plumbing touches the hot claim path; mitigated by reusing
  `lease_generation` instead of introducing a second counter.
- Retention default flip is the only behavior change visible to existing
  deployments; dry-run-first sequencing bounds it to reporting until
  acknowledged.
- The version predicate (D2) can surface latent writers that bypass the
  helper today; that surfacing is the point, but rollout should watch for
  `InstanceVersionConflict` in logs during the first release.
