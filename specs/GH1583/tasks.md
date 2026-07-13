# GH1583 Task Plan

## Linked Issue

GH-1583

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [x] `SP1583-T001` Owner: `config` | Done when: `PriorityAgingConfig { enabled: true, interval_secs: 300, max_boost_levels: 2 }` lands in `crates/harness-core/src/config/misc.rs`, nested in `ConcurrencyConfig` with `#[serde(default)]`; load-time validation rejects `enabled = true` with `interval_secs = 0`; empty-TOML deserialization keeps defaults | Verify: `cargo test -p harness-core config`
- [x] `SP1583-T002` Owner: `queue-core` | Done when: `Waiter` carries `enqueued_at: tokio::time::Instant`; `waiters` becomes `Vec<Waiter>`; `release()` selects by `(effective_priority DESC, seq ASC)` via lazy scan with cap `min(base + max_boost_levels, max level)`; `drain_cancelled` uses `Vec::retain`; aging-off path selects identically to the current BinaryHeap comparator; all 24 existing `task_queue_tests.rs` tests pass unmodified | Verify: `cargo test -p harness-workflow task_queue`
- [x] `SP1583-T003` Owner: `aging-tests` | Done when: paused-clock tests cover B-001/B-002/B-004/B-006/B-007 — boost-per-interval and cap; steady max-priority stream cannot delay a priority-0 waiter past the per-stage bound; aged tie breaks by seq; cancelled aged waiter never granted and excluded from metrics; two-stage bound documented and asserted; plus `aging_off_matches_legacy_order` (B-003) | Verify: `cargo test -p harness-workflow task_queue`
- [x] `SP1583-T004` Owner: `metrics` | Done when: `PriorityPermitQueue` records grant count / max wait / p95 (ring buffer) per base priority level at grant time only; `QueueStats` and `QueueDiagnostics` expose `wait_ms_by_priority` additively; server consumers (`dashboard.rs:165`, `dashboard_active_counts.rs:169`, `misc_routes.rs:118`, `execution.rs:249`) compile unchanged or with additive plumbing | Verify: `cargo test -p harness-workflow task_queue && cargo check -p harness-server`
- [x] `SP1583-T005` Owner: `wiring` | Done when: `TaskQueue::new`/`new_with_pressure` thread `ConcurrencyConfig.aging` into the global queue and every lazily created project queue; `TaskQueue::acquire` docs state the per-stage aging bound and that end-to-end bound is the sum of both stages | Verify: `cargo test -p harness-workflow task_queue && cargo check --workspace`

## Parallelization

- `SP1583-T001` (harness-core) and `SP1583-T002` (harness-workflow) touch
  disjoint crates and can run in parallel; `T002` uses hardcoded aging
  params in unit scope until `T005` wires config through.
- `SP1583-T003` and `SP1583-T004` both edit `task_queue.rs`/
  `task_queue_tests.rs` — keep serial after `T002`.
- `SP1583-T005` last, after `T001` and `T004`.

## Verification

- `cargo test -p harness-core config`
- `cargo test -p harness-workflow task_queue`
- `cargo check --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`

## Handoff Notes

Aging ships ON by default with a conservative slope (300s per level, cap
+2). If field reports show unwanted reordering, the operator fallback is
`[concurrency.aging] enabled = false`, which restores today's semantics
exactly (B-003) — record any such downgrade decision in this packet.
Priority inversion by permit holders remains out of scope; if it recurs,
open a separate issue rather than widening this one.
