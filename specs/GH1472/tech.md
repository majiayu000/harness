# Tech Spec

## Linked Issue

GH-1472

## Product Spec

See `specs/GH1472/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Task schema | `crates/harness-server/src/task_db/migrations.rs` | `tasks`, `task_artifacts`, `task_prompts`, and `task_checkpoints` are scoped by `store_key` in migration v26. `task_artifacts` already has `idx_task_artifacts_store_task_id ON task_artifacts(store_key, task_id, turn, id)`. | The issue's index request should be verified against current schema rather than duplicating the existing scoped index. |
| Artifact query | `crates/harness-server/src/task_db/queries_aux.rs` | `list_artifacts` filters by `store_key = $1 AND task_id = $2` and orders by `id ASC`. | The existing index covers the filter and provides deterministic scan order with `turn, id`; tests should protect it. |
| Prompt/checkpoint tables | `crates/harness-server/src/task_db/queries_aux.rs` | Prompt and checkpoint rows are written and loaded by `store_key` and `task_id`. No retention delete path exists. | These rows must be pruned with their parent task. |
| Task statuses | `crates/harness-server/src/task_runner/types.rs` | Terminal statuses are `done`, `failed`, and `cancelled`; resumable statuses are separate. | Retention must use the canonical terminal status list and never infer terminal rows ad hoc. |
| Workflow storage config | `crates/harness-core/src/config/workflow.rs`, `crates/harness-core/src/config/workflow_tests.rs` | Storage config already contains orphan reaper, workflow watchdog, and runtime retention settings. Runtime retention defaults disabled. | Task retention should live beside existing storage retention knobs and follow the same default posture. |
| Runtime retention loop | `crates/harness-server/src/http/runtime_retention.rs`, `crates/harness-server/src/http/mod.rs` | A background loop reloads workflow config, respects an enabled flag, computes a cutoff, prunes bounded batches, and logs non-empty summaries. | This is the closest local pattern for task retention scheduling. |
| Docs/examples | `WORKFLOW.md`, `config/WORKFLOW.md`, `docs/usage-guide.md`, `config/default.toml.example` | Existing docs show storage retention settings for workflow-runtime history. | Operators need the new task retention knobs documented in the same location. |

## Proposed Design

1. Preserve and verify the artifact index.
   - Do not add a duplicate `task_artifacts(task_id)` index because scoped
     stores query by `store_key` and `task_id`.
   - Add a regression test that v26 contains
     `idx_task_artifacts_store_task_id` and that the artifact query predicate
     matches `store_key` plus `task_id`.
   - Where a Postgres integration test is available, use `EXPLAIN` for the
     artifacts-by-task query after enough rows are inserted and assert the plan
     uses an index scan.
2. Add task-retention workflow storage config.
   - Add fields such as:
     - `task_retention_enabled: bool` default `false`
     - `task_retention_days: u64` default `30`
     - `task_retention_batch_size: u32` default `1000`
     - `task_retention_interval_secs: u64` default `3600`
   - Update `Default`, front-matter parsing tests, `WORKFLOW.md`,
     `config/WORKFLOW.md`, `docs/usage-guide.md`, and
     `config/default.toml.example`.
3. Add a task retention query surface.
   - Add a small summary type, for example `TaskRetentionPruneSummary`, with
     counts for tasks, artifacts, prompts, and checkpoints deleted.
   - Add `TaskDb::prune_terminal_tasks_before(cutoff, batch_size)`.
   - Select eligible task IDs in a bounded CTE:
     `store_key = $1`, `status IN terminal_statuses`, `updated_at < $2`,
     ordered by `updated_at ASC, id ASC`, limited by `$3`.
   - Delete child rows first using the selected IDs and `store_key`, then
     delete `tasks` using the same selected IDs.
   - Run in a transaction so child and parent deletes stay consistent.
4. Wire a background task retention loop.
   - Add a focused server module, for example
     `crates/harness-server/src/http/task_retention.rs`.
   - Spawn it from `http/mod.rs` near the existing runtime retention spawner.
   - Reload workflow config each tick, respect the enabled flag, compute the
     cutoff with `Utc::now() - task_retention_days`, and clamp interval and
     batch size to safe minimums.
   - Log a structured info message only when the summary is non-empty; log
     warnings on failed ticks without swallowing the error silently.
5. Keep retention isolated from active behavior.
   - Use `TaskStatus::terminal_statuses()` or a local helper derived from it,
     not string duplication across multiple query sites.
   - Do not touch task caches, runtime workflow stores, workspace cleanup, or
     GitHub reconciliation in this issue.

## Data Flow

The server opens `TaskDb` during storage initialization as it does today. The new
retention loop periodically loads `WORKFLOW.md`, checks
`storage.task_retention_enabled`, computes a cutoff timestamp, and calls
`TaskDb::prune_terminal_tasks_before`.

The database method selects a batch of eligible terminal task IDs for the
current `store_key`. It deletes matching artifacts, prompts, and checkpoints,
then deletes matching tasks, all inside one transaction. The returned summary is
logged by the loop.

## Alternatives Considered

- Add a bare `task_artifacts(task_id)` index. Rejected because current scoped
  schema queries include `store_key`; a task-id-only index is less selective and
  duplicates the existing v26 scoped index intent.
- Reuse runtime retention settings for task rows. Rejected because task rows
  and workflow-runtime families have different data ownership and rollout risk.
- Enable task retention by default. Rejected for first rollout because deleting
  task history is irreversible without database backups.
- Add database cascade constraints. Deferred because the current schema does
  not define FK constraints for these legacy task-owned tables; explicit delete
  ordering is smaller and easier to audit.

## Risks

- Overbroad deletes could remove active work. Mitigate by filtering only the
  canonical terminal status list and by testing active/resumable statuses.
- A shared-schema retention bug could delete another store's rows. Mitigate by
  binding `store_key` in every selection and delete.
- Large deletes could hold locks too long. Mitigate with a configurable batch
  size and ordered batches.
- Operators may expect task history for dashboards or proof views. Mitigate by
  keeping retention disabled by default and documenting the irreversible history
  trade-off.

## Test Plan

- [ ] `cargo test -p harness-server --test task_db_retention`
- [ ] `cargo test -p harness-server task_db::migrations`
- [ ] `cargo test -p harness-core config::workflow`
- [ ] `cargo check -p harness-server --all-targets`
- [ ] `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1472`
- [ ] `python3 checks/check_workflow.py --repo .`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`

## Rollback Plan

Disable `storage.task_retention_enabled` to stop future pruning. If the
implementation needs to be removed, revert the implementation commit; already
deleted task history can be restored only from database backups.
