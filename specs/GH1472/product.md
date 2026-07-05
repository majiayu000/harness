# Product Spec

## Linked Issue

GH-1472

## User Problem

Task-owned storage grows without a row-retention policy. Terminal tasks keep
their task rows plus per-task artifacts, prompts, and checkpoints indefinitely,
so operators that run Harness continuously accumulate historical rows that no
longer participate in active execution.

The artifacts-by-task read path must also remain indexed. The current shared
schema migration already creates `idx_task_artifacts_store_task_id` over
`(store_key, task_id, turn, id)`, which matches the scoped artifact query, but
this should be verified so future schema changes do not regress into sequential
scans.

## Goals

- Keep `list_artifacts(task_id)` backed by an index that matches the
  `store_key` and `task_id` predicate.
- Add an explicit, configurable retention policy for terminal task-owned rows.
- Prune task child tables in FK-safe order before deleting eligible terminal
  task rows.
- Keep retention disabled by default or conservative enough for safe rollout.
- Document the configuration next to the existing workflow/runtime retention
  settings.

## Non-Goals

- Archiving, export, or restore tooling for pruned task history.
- Changing live task query paths, active task lifecycle behavior, or scheduler
  ownership rules.
- Pruning workflow-runtime history beyond the existing runtime retention path.
- Deleting non-terminal, resumable, pending, or dependency-blocked tasks.

## User-Visible Behavior

When task retention is disabled, Harness behaves as it does today and keeps all
task-owned rows.

When retention is enabled, Harness periodically deletes terminal tasks older
than the configured window, along with matching artifacts, prompts, and
checkpoints. Active and resumable tasks remain visible and recoverable.

Configuration appears in `WORKFLOW.md` and usage docs near existing storage
retention settings, so operators can enable task retention intentionally.

## Acceptance Criteria

- [ ] The artifacts-by-task query remains backed by a scoped
      `(store_key, task_id, ...)` index, with an `EXPLAIN` or migration
      regression test proving the query can use an index scan.
- [ ] A task retention config block exists under workflow storage settings,
      defaults disabled or conservatively, and is documented in repo examples.
- [ ] Retention chooses only terminal task statuses older than the configured
      cutoff.
- [ ] Retention deletes dependent `task_artifacts`, `task_prompts`, and
      `task_checkpoints` rows before deleting their parent `tasks` rows.
- [ ] Retention runs in bounded batches and logs non-empty prune summaries.
- [ ] Tests cover terminal rows being pruned, active/resumable rows being kept,
      and child rows being removed with the parent task.

## Edge Cases

- `cancelled` is terminal and should be eligible after the retention window.
- `pending`, `awaiting_deps`, and all resumable in-flight statuses must never be
  pruned by the retention query.
- Batch size `0` should not create an unbounded delete; use a minimum safe batch
  or treat it as disabled.
- Retention should be scoped by `store_key` so one shared-schema store cannot
  delete another store's rows.
- Missing or invalid workflow config should not trigger pruning with stale
  values.

## Rollout Notes

Use `Refs #1472` for the spec PR. The implementation PR should use
`Closes #1472` after the index verification, retention config, pruning logic,
docs, and tests land.
