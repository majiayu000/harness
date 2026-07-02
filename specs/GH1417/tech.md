# Tech Spec

## Linked Issue

GH-1417

## Product Spec

See `specs/GH1417/product.md`.

## Current System

- `blocked` has one modeled exit, `blocked -> done` (`crates/harness-workflow/src/runtime/validator_github_issue_pr.rs:15`); projection maps it to `Waiting` (`crates/harness-server/src/runtime_projection.rs:91`). No sweeper re-activates or escalates aged instances.
- `periodic_retry.rs:72-335` implements retries/stuck handling for the legacy task model only. `runtime_summary_counts_for_instances` (`crates/harness-workflow/src/runtime/store_summary.rs:6`) counts states but no consumer alerts on age.
- Stall detection for tasks is computed on operator-endpoint request with a 30-minute threshold (`crates/harness-server/src/handlers/operator_monitor.rs:29,142-145`) — pull-only.
- `runtime_events`, `workflow_events`, `runtime_jobs` have no delete path; `compact_runtime_jobs_for_commands_limited` (`crates/harness-workflow/src/runtime/store.rs:1685`) is a read-side limiter.
- Sweeper prior art: pr_feedback / pr_hygiene sweepers and the orphan schema reaper are spawned at startup from `crates/harness-server/src/http/mod.rs:243-260`.
- Existing recovery precedent: stale recovery is routed through decision records (commit `c6c4c15a` "route stale recovery through decisions"), which is the pattern the watchdog must follow for any mutating action.

## Proposed Design

Two sweepers, one new store surface, one operator endpoint.

Watchdog sweeper (`spawn_workflow_watchdog` in harness-server, following the orphan-reaper shape):

1. New store query `list_aged_wait_instances(states, older_than, limit)` in `harness-workflow/src/runtime/store.rs` — indexed on `(state, updated_at)`; add the index in a new forward-only migration in `store_migrations.rs`.
2. Each tick: fetch aged instances in `blocked` / `awaiting_feedback`; for each, `error!`-log (workflow id, definition, state, age, bound repo/issue) and upsert into an in-memory + queryable snapshot used by the operator API.
3. Optional recovery hook (second phase, config-gated separately): for definitions where a safe route exists, enqueue a recovery decision through the existing reducer/decision machinery — never mutate instance state directly from the sweeper.
4. Operator surface: `GET /api/operator-monitor` gains a `stuck_workflows` section (extend `handlers/operator_monitor.rs` and the web `operator_snapshot` types), so the web cockpit shows it without a new page.

Retention sweeper (`spawn_runtime_retention`):

1. New store mutation `prune_terminal_runtime_history(terminal_before, batch_limit)` deleting from `runtime_jobs`, `runtime_events`, `workflow_events` for instances whose terminal transition is older than the window — guarded by a family check (parent and all children terminal) implemented as a single SQL predicate over the parent/child linkage.
2. Batched deletes (`LIMIT` per tick) inside one transaction per batch; returns deleted counts which the sweeper logs per tick.
3. Config validation: `retention_days` must be >= reconciliation/summary lookback windows or startup emits a warning naming both values.

Config (new `[runtime.sweepers]` section or alongside existing storage config): `watchdog_enabled` (default false), `watchdog_age_minutes` (default 240), `retention_enabled` (default false), `retention_days` (default 30), `retention_batch_size` (default 1000), tick intervals.

## Data Flow

- Inputs: workflow instance/event/job tables; config thresholds.
- Outputs: error-level logs, operator-monitor JSON section, decision records (recovery hook only), batched DELETEs.
- External calls: none. Both Postgres and SQLite paths must implement the queries (same sqlx surface as existing store code).

## Alternatives Considered

- Extend `periodic_retry.rs` to also cover workflow instances: rejected — that module is legacy-task-scoped and already 2.4k+ lines-adjacent territory; runtime concerns belong in runtime-owned sweepers.
- Event-table partitioning (Postgres native partitions) instead of delete-based retention: better at very large scale but heavier migration and no SQLite story; revisit if delete batches prove insufficient.
- Watchdog auto-transitions stuck instances (e.g. force `blocked -> failed`): rejected — mutating state outside the validator/decision path breaks the event-sourced audit trail; the decision-routed recovery hook is the only mutation channel.

## Risks

- Security: none new; operator endpoint already behind API auth.
- Compatibility: off-by-default flags preserve existing behavior; pruning only touches terminal families, and the family predicate is the main correctness risk — covered by dedicated tests.
- Performance: aged-instance query needs the new index; deletes are batched. First-enable backlogs bounded by batch size.
- Maintenance: two more sweepers in `http/mod.rs` startup — acceptable, follows the established pattern; keep each in its own module rather than growing `background.rs`.

## Test Plan

- [ ] Unit tests: aged-instance query boundary conditions (threshold, terminal exclusion); family-terminal predicate (active child blocks pruning); batch-limit behavior.
- [ ] Integration tests: watchdog tick surfaces a seeded aged `blocked` instance into the operator snapshot and error log; retention tick prunes a terminal family and leaves an active one intact (both Postgres via test harness and SQLite).
- [ ] Manual verification: enable both sweepers on a dev deployment seeded with aged instances; capture row counts before/after and the operator-monitor payload.

## Rollback Plan

Disable via config flags (`watchdog_enabled = false`, `retention_enabled = false`) — no code rollback needed. Pruned history is not recoverable; that is why retention ships off-by-default and after the watchdog, with the batch/window guards above.
