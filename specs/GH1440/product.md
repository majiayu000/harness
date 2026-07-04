# Product Spec

## Linked Issue

GH-1440

## User Problem

Operators comparing Harness status surfaces during an incident can see
contradictory active-work counts. `GET /projects/queue-stats` reports the
in-memory task queue as empty while `GET /api/overview` reports many queued and
running workflow-runtime tasks. At the same time, `GET /health` reports a
runtime log `path_hint` such as `logs/harness-serve-...log`, but the server is
actually writing the log under the configured runtime data directory, so the
hint can point to a nonexistent project-relative path.

When these endpoints disagree, operators cannot tell which status surface is
trustworthy without querying the database or inspecting open file handles.

## Goals

- `/projects/queue-stats` and `/api/overview` use the same active-work source
  for global and per-project running/queued counts.
- Workflow-runtime-only work is visible in queue stats, not only legacy task
  queue waiters.
- Runtime log metadata returned by `/health` points to the actual active log
  path when runtime logging is enabled.
- Existing response shapes remain compatible unless a field is corrected to
  represent the real source of truth.

## Non-Goals

- Redesigning the dashboard, overview page, or operator monitor UI.
- Changing workflow scheduling, queue admission, or task execution semantics.
- Adding new runtime persistence tables.
- Rotating or retaining runtime logs; that belongs to log-retention work.
- Exposing log file contents through the API.

## User-Visible Behavior

When workflow-runtime instances are active but the in-memory legacy queue is
empty, `GET /projects/queue-stats` reports the same active running and queued
counts that `GET /api/overview` uses for its active-task KPIs. Project entries
are keyed by the same project identifiers used by the active-work projection.

When runtime logging is enabled, `GET /health` returns a `runtime_logs.path_hint`
that is a usable path to the actual active log file. If logging is disabled,
the hint remains absent. If log setup is degraded before an active path exists,
the degraded hint may remain the best non-secret setup hint available.

## Acceptance Criteria

- [ ] `GET /projects/queue-stats` global `running` and `queued` counts are
      derived from the same canonical active-work aggregation as
      `GET /api/overview`.
- [ ] Queue stats include nonterminal workflow-runtime instances, including
      workflows with no legacy task row.
- [ ] Queue stats preserve per-project `running`, `queued`, and `limit` fields.
- [ ] Existing queue stats capacity fields continue to report configured queue
      limits rather than active-work counts.
- [ ] `GET /health` reports the actual active runtime log path when runtime
      logging is enabled.
- [ ] Tests cover the queue-stats/overview consistency path and the corrected
      runtime log path hint.

## Edge Cases

- Legacy active tasks and workflow-runtime instances that reference the same
  task must not be double counted.
- Terminal legacy tasks must not suppress active workflow-runtime work.
- If active-task persistence is unavailable, queue stats may fall back to the
  in-memory queue, but the response must not claim workflow-runtime-derived
  counts in that fallback case.
- Project identifiers may be absent for some active work; global counts still
  include those rows.
- Log paths on macOS may live outside the project root under Application
  Support; the health hint must not invent a project-relative location.

## Rollout Notes

This is an observability correctness fix. It changes status reporting only and
does not alter workflow scheduling, admission control, runtime execution, or
merge/review gates.
