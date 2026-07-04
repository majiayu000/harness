# Tech Spec

## Linked Issue

GH-1472

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Migrations | `crates/harness-server/src/task_db/migrations.rs:54-62` | task_artifacts PK only | index migration target |
| Query | `crates/harness-server/src/task_db/queries_aux.rs:88` | WHERE task_id = $1 | seq scan today |
| Tables | tasks, task_artifacts, task_prompts, task_checkpoints | no DELETE anywhere | retention targets |
| Precedent | `harness-observe/src/event_store/mod.rs:140-176` | events retention exists | pattern to mirror |
| Periodic loops | `crates/harness-server/src/scheduler.rs` | hourly+ ticks | host for prune job |

## Proposed Design

1. Migration: `CREATE INDEX ... ON task_artifacts(task_id)`; audit
   task_prompts/task_checkpoints for the same predicate and index alike.
2. Config `storage.task_retention_days` (default: disabled/None to be
   conservative) + `storage.retention_batch_size`.
3. Prune job on the scheduler: select terminal tasks older than the window,
   delete dependents first (artifacts, prompts, checkpoints), then the task
   row, batched, per-run counts logged at info.
4. Mirror the events-retention structure for consistency.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 terminal+aged only | prune query | test: young/live tasks survive |
| P2 FK-safe order | delete sequence | test with artifacts+prompts attached |
| P3 observable | prune logging | test asserts per-table counts |

## Alternatives Considered

- Partitioning by month — heavier; revisit if volume demands it.
- Default-on retention — rejected: silent data deletion needs explicit opt-in.
