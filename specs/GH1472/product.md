# Product Spec

## Linked Issue

GH-1472

## User Problem

`task_artifacts` is queried by task_id with no index (sequential scan), and
tasks/task_artifacts/task_prompts/task_checkpoints grow forever with no
retention. Query latency and storage degrade linearly with task volume.

## Goals

- Index the artifacts-by-task access path.
- Bounded row growth for task tables via configurable retention of
  terminal-state tasks.

## Non-Goals

- Archival/export tooling.
- Schema-level growth (covered by GH-1436 / GH-1461).

## Behavior Invariants

1. Retention only ever removes tasks in terminal states older than the
   configured window, and their dependent rows, in FK-safe order.
2. Live/non-terminal tasks and their rows are never pruned.
3. Retention is observable: each prune run logs counts per table.
