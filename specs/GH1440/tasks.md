# Task Plan

## Linked Issue

GH-1440

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1440-T001` Owner: `status-counts` | Done when: the active-work aggregation used by `/api/overview` is callable from `/projects/queue-stats` without duplicating legacy/runtime dedupe logic | Verify: `cargo test -p harness-server overview active_counts`
- [ ] `SP1440-T002` Owner: `queue-stats` | Done when: `/projects/queue-stats` reports canonical active running/queued counts globally and per project while preserving configured limit fields | Verify: `cargo test -p harness-server queue_stats`
- [ ] `SP1440-T003` Owner: `runtime-logs` | Done when: enabled runtime log metadata exposes the actual active log path through `/health` `runtime_logs.path_hint`, with disabled/degraded cases preserved | Verify: `cargo test -p harness-server health_runtime_logs`
- [ ] `SP1440-T004` Owner: `verification` | Done when: focused tests, package check, workflow check, fmt, and clippy pass for the changed surface | Verify: `cargo test -p harness-server queue_stats && cargo test -p harness-server health_runtime_logs && cargo check -p harness-server --all-targets && python3 checks/check_workflow.py --repo . --spec-dir specs/GH1440`

## Parallelization

T001 and T002 should be sequenced because queue-stats depends on the shared
active-count source. T003 touches runtime log metadata and health tests, so it
can be implemented independently from T001/T002. T004 is the final verification
gate.

## Verification

- `cargo test -p harness-server queue_stats`
- `cargo test -p harness-server health_runtime_logs`
- `cargo check -p harness-server --all-targets`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1440`
- `python3 checks/check_workflow.py --repo .`

## Handoff Notes

Keep this fix scoped to status correctness. Do not change scheduler admission,
workflow-runtime state transitions, runtime log rotation, or dashboard UI
layout. The central invariant is that `/projects/queue-stats` and
`/api/overview` must not report contradictory active running/queued counts for
the same server state.
