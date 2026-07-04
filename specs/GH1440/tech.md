# Tech Spec

## Linked Issue

GH-1440

## Product Spec

See `specs/GH1440/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Queue stats route | `crates/harness-server/src/http/misc_routes.rs` | `project_queue_stats` reads only `state.concurrency.task_queue` for global and per-project counts. | This misses workflow-runtime work and produced the observed `0/0` count. |
| Overview active counts | `crates/harness-server/src/handlers/overview.rs` | `active_task_overview_counts` merges legacy active task summaries and nonterminal workflow-runtime instances, with an in-memory queue fallback only when both persisted sources are unavailable. | This is the count source that produced the nonzero active-work numbers. |
| Dashboard active counts | `crates/harness-server/src/handlers/dashboard_active_counts.rs` | Dashboard status already has a richer active-work helper that avoids double counting and can include queue waiters. | This is the nearest existing pattern for canonical status counts. |
| Runtime log metadata | `crates/harness-server/src/server.rs` | `RuntimeLogMetadata::public_path_hint` converts an active log path into `logs/<filename>`. | On installations whose logs live outside the project root, that hint is not usable. |
| Health route tests | `crates/harness-server/src/http/tests/health_route_tests.rs` | Current tests require `/health` to redact the absolute active log path. | These tests encode the behavior that conflicts with GH-1440. |

## Proposed Design

1. Share the active-work count source between overview and queue stats.
   - Make the overview active-work aggregation available to the queue-stats
     route, or extract the shared logic into a small status-count helper.
   - Preserve dedupe behavior between legacy task summaries and
     workflow-runtime projections.
   - Keep the queue capacity fields (`global.limit` and per-project `limit`)
     sourced from the in-memory task queue configuration.
2. Update `GET /projects/queue-stats`.
   - Build `global.running` and `global.queued` from canonical active-work
     counts, not from `TaskQueue::running_count()` and `queued_count()` alone.
   - Build per-project `running` and `queued` from the same canonical
     `by_project` map.
   - Preserve project `limit` from `TaskQueue::project_stats` where available;
     use the configured global limit for projected projects that have no
     task-queue entry.
3. Correct runtime log path hints.
   - Change enabled runtime log metadata so `path_hint` is the actual active
     path string.
   - Keep disabled runtime logs as `path_hint: null`.
   - For degraded setup with no active path, continue to return only the
     degraded setup hint already available, avoiding raw setup error text.
4. Update tests.
   - Add a queue-stats route test that seeds a workflow-runtime active
     instance with no legacy task row and asserts queue stats report it.
   - Add or adjust an overview consistency assertion so queue-stats and
     overview active counts agree for the seeded active work.
   - Update health runtime-log tests to assert the actual active path rather
     than a project-relative synthetic hint.

## Data Flow

The status path should derive active work from persisted task summaries plus
workflow-runtime projections. The active count helper classifies each item as
running or queued, dedupes workflow-runtime instances that are represented by
an active legacy task row, and returns global plus per-project counts.

`/api/overview` uses these counts for `kpi.active_tasks` and related global
running/queued fields. `/projects/queue-stats` uses the same counts for
`global.running`, `global.queued`, and project count fields while preserving
capacity metadata from the task queue.

Runtime log metadata is constructed during server bootstrap. Once an active log
path exists, the server stores that exact path as the public health hint so the
health response points to the file operators can inspect.

## Alternatives Considered

- Document `/projects/queue-stats` as legacy-queue-only. Rejected because the
  endpoint name is used as a status surface and operators need the active
  workflow-runtime view during incidents.
- Make `/api/overview` read only the in-memory task queue. Rejected because it
  would hide workflow-runtime work and regress the more complete surface.
- Add a second health field for the absolute runtime log path while leaving
  `path_hint` project-relative. Rejected because the bug is specifically that
  `path_hint` points to a nonexistent location.

## Risks

- Compatibility: clients may have assumed queue stats were legacy queue-only.
  Preserve field names and capacity fields to minimize response-shape churn.
- Privacy: `/health` will expose a local absolute log path. This is necessary
  for the issue, and it must expose only a path, never log contents or setup
  error text.
- Counting correctness: active legacy tasks and workflow-runtime projections
  can represent the same work. Reuse existing dedupe logic instead of adding a
  parallel ad hoc projection.

## Test Plan

- [ ] Handler/helper tests: active workflow-runtime work with no legacy task row
      appears in the canonical active counts.
- [ ] Route tests: `/projects/queue-stats` reports the same active global
      running/queued values as `/api/overview` for seeded workflow-runtime
      work.
- [ ] Route tests: per-project queue stats include workflow-runtime active
      counts and keep capacity fields.
- [ ] Health tests: enabled runtime logs return the actual active path in
      `runtime_logs.path_hint`.
- [ ] Regression tests: disabled/degraded runtime log metadata remains safe and
      does not expose raw setup errors.

## Rollback Plan

Revert the route/helper changes and runtime log metadata change. No data
migration is involved.
