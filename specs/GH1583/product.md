# GH1583 Product Spec: Priority Aging / Anti-Starvation for PriorityPermitQueue

GitHub issue: `#1583`

## Goals

- Bound the wait time of low-priority tasks under sustained high-priority
  load so unattended maintenance loops (quality scans, GC remediation,
  learn loops) cannot stall silently.
- Make queue wait time observable per priority level through the existing
  queue stats/diagnostics surface.
- Ship aging ON by default with a conservative slope; `aging off` must
  preserve today's ordering semantics exactly.

## Non-Goals

- No fix for priority inversion caused by a permit *holder* (a running
  low-priority task blocking a high-priority waiter) — aging only affects
  waiters, not preemption.
- No changes to the priority model itself (`0..=2`, DB column, request
  validation) or to `max_queue_size` admission.
- No new external metrics backend; reuse the existing stats/diagnostics
  path only.

## Users

- Fleet operators running Harness unattended, where background maintenance
  tasks are submitted at priority 0 while interactive work arrives at 1-2.
- Dashboard consumers who need to see queue pressure and wait-time
  distribution per priority level.

## Behavior Invariants

- B-001: With aging enabled, a waiter's effective priority increases by one
  level per configured `interval_secs` of wait, capped at
  `base + max_boost_levels` (and never above the maximum priority level).
- B-002: Under a steady stream of maximum-priority arrivals, a priority-0
  waiter is granted a permit within the deterministic aging bound
  (`max_boost_levels * interval_secs` plus one FIFO turn at the capped
  level) per queue stage, verified with a mocked/paused clock.
- B-003: With aging disabled (`aging.enabled = false`), grant order is
  identical to current behavior (priority DESC, FIFO within level) for any
  arrival sequence; existing ordering tests pass unchanged.
- B-004: Ties in effective priority (including ties created by aging) break
  by enqueue sequence — the earlier waiter wins, deterministically.
- B-005: Aging uses injected/monotonic time (tokio paused-clock compatible);
  no wall-clock reads, so clock adjustments cannot reorder waiters.
- B-006: A waiter cancelled mid-wait (future dropped) is never granted a
  permit by aging, never leaks a permit, and is excluded from wait-time
  metrics.
- B-007: Aging applies independently to the project-stage and global-stage
  queues; the documented end-to-end bound is the sum of both stage bounds.
- B-008: Queue stats expose per-priority-level wait-time metrics (max and
  p95 in milliseconds, grant count) for granted waiters, via the existing
  `QueueStats`/`QueueDiagnostics` surface.
- B-009: Config defaults are aging ON with a conservative slope; an
  unconfigured `[concurrency.aging]` section deserializes to those defaults
  and an existing config file without the section keeps loading.
- B-010: Invalid aging config (e.g. `interval_secs = 0` with aging enabled)
  is rejected at config load with an error, not silently clamped.

## Boundary Checklist

| Category | Coverage |
|---|---|
| Empty input | covered: B-009 (missing config section), plus empty-queue release path unchanged (B-003) |
| Failure paths | covered: B-010 (invalid config rejected loudly) |
| Authorization | N/A + reason: in-process scheduler; no auth boundary — priority validation stays in `execution.rs` request validation |
| Concurrency | covered: B-002, B-004 (deterministic order under concurrent waiters, paused-clock tests) |
| Retry + idempotency | covered: B-006 (release/reclaim CAS path unchanged; re-enqueue after cancel gets a fresh enqueue time) |
| Illegal transitions | covered: B-001 (effective priority monotonically non-decreasing, hard cap; can never exceed max level) |
| Compatibility | covered: B-003, B-009 (aging off is byte-identical semantics; old config files load) |
| Degradation / fallback | covered: B-010 (no silent clamp); disabling aging is the explicit operator fallback (B-003) |
| Evidence / audit | covered: B-008 (wait-time metrics per level on the diagnostics surface) |
| Cancellation / partial | covered: B-006, B-007 (mid-wait drop at either stage; project permit released as today) |

## Acceptance Criteria

- Deterministic test with the tokio paused clock: a steady high-priority
  stream cannot delay a low-priority task beyond the configured aging bound
  (B-002).
- `aging.enabled = false` reproduces today's ordering exactly; all existing
  `task_queue_tests.rs` ordering/cancellation tests pass unchanged (B-003).
- Wait-time metrics (max/p95 per priority level) visible through
  `project_stats`/`diagnostics` consumers (B-008).
- Aging is ON by default with conservative slope (B-009).
