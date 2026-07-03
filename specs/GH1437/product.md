# Product Spec

## Linked Issue

GH-1437

## User Problem

`Agent turn timed out after 3600s` is the dominant task-failure reason (14 of 28 failed tasks, 2026-07-03). When an upstream connection dies (observed with transient proxy/TUN drops), the turn produces no further output, yet the worker slot is held for a full hour before the task fails opaquely. During a network incident the whole worker pool degrades into hour-long zombies.

## Goals

- A silent (no-output) turn is aborted within a bounded inactivity window that is meaningfully shorter than the wall-clock ceiling.
- Stall aborts are distinguishable from genuine long-turn wall-clock timeouts in task failure reasons and logs, so they can feed failure-class accounting (#1430).
- Every agent execution path (task executor and workflow runtime jobs, all runtime kinds) has stall protection.

## Non-Goals

- Changing the wall-clock `timeout_secs` semantics or defaults.
- Retry-policy or circuit-breaker behavior (#1430).
- Detecting semantically-stuck turns that keep emitting output.

## Behavior Invariants

1. A turn that emits no stream items for the configured stall window is failed with a stall-specific reason naming the silence duration, before the wall-clock timeout fires.
2. The default stall window is strictly smaller than the default wall-clock timeout (a stall default equal to the wall-clock default is a misconfiguration and must be impossible to ship silently).
3. Any stream activity (output, tool events, errors) resets the stall clock; a turn steadily producing output can run up to the wall-clock limit.
4. Stall failures and wall-clock failures produce distinct failure reasons, both visible in task status and logs.
5. Stall protection applies uniformly across execution paths (task executor turns and workflow runtime jobs) and runtime kinds (claude, codex, codex_exec).
6. Per-task/activity override of the stall window remains possible; an override larger than the wall-clock timeout is clamped or rejected with a warning.

## Acceptance Criteria

- [ ] With a simulated silent agent stream, the turn fails with a stall reason within the stall window (not after 3600 s).
- [ ] Failure reason strings differ between stall and wall-clock timeout; both appear in `GET /tasks` failure output.
- [ ] Default config ships a stall window ≤ 15 min while wall-clock stays 3600 s; `config/default.toml.example` documents both.
- [ ] Workflow runtime (codex_exec) jobs demonstrate the same stall abort in a test.

## Edge Cases

- Agent emits only heartbeat/noise events but no substantive output — still counts as activity (v1 accepts this; semantic stalls are out of scope).
- Stall fires exactly while the final result block is being flushed — result arriving after abort must not resurrect the turn.
- Clock: stall window measured from last received item, not turn start.
- Override set to 0 or absurdly small — enforce a sane floor (e.g. 60 s).

## Rollout Notes

Behavior change: previously-silent hour-long hangs now fail after the stall window. Deployments with legitimately silent long-running agents (none known) can raise `concurrency.stall_timeout_secs`. Announce the new default in the changelog; no data migration.
