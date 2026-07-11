# GH1583 Tech Spec: Priority Aging / Anti-Starvation for PriorityPermitQueue

Product spec: `specs/GH1583/product.md`
GitHub issue: `#1583`

## Codebase Context (verified anchors)

- `crates/harness-workflow/src/task_queue.rs:122-126` —
  `PriorityPermitQueue { available, next_seq, waiters: BinaryHeap<Waiter> }`.
- `crates/harness-workflow/src/task_queue.rs:68-109` — `Waiter { priority: u8,
  seq: u64, tx, permit_granted, cancelled }` with `Ord` = priority DESC,
  seq ASC (FIFO within level).
- `crates/harness-workflow/src/task_queue.rs:117-121` — doc comment naming
  priority inversion and low-priority starvation as unhandled follow-ups
  (this issue).
- `crates/harness-workflow/src/task_queue.rs:143-162` —
  `try_acquire_or_enqueue(priority)` pushes a `Waiter` when no slot is free.
- `crates/harness-workflow/src/task_queue.rs:171-221` — `release()` pops the
  top waiter, skips `cancelled` entries, CAS-reclaims on failed send.
- `crates/harness-workflow/src/task_queue.rs:231-234` — `drain_cancelled()`
  (`BinaryHeap::retain`).
- `crates/harness-workflow/src/task_queue.rs:539-638` — two-stage
  `TaskQueue::acquire(project_id, priority)`: project queue first, then
  global; cancellation-safe via `AcquireGuard` (`:283-402`).
- `crates/harness-workflow/src/task_queue.rs:15-23, 694-757` — `QueueStats`
  (Serialize), `diagnostics()`, `project_stats()`.
- `crates/harness-workflow/src/task_queue_tests.rs` — 625 lines; ordering
  tests `high_priority_acquired_before_low_when_slots_full` (`:251`),
  `same_priority_is_fifo` (`:287`); cancellation tests (`:351-513`); all
  timing tests use `#[tokio::test(start_paused = true)]` (paused clock).
  `tokio = { features = ["test-util"] }` already in dev-deps
  (`crates/harness-workflow/Cargo.toml:34`).
- `crates/harness-core/src/config/misc.rs:110-158` — `ConcurrencyConfig`
  with serde defaults (`:160-197`); target home for the aging sub-config.
- Priority origin: `crates/harness-server/src/task_runner/request.rs:9`
  (`MAX_TASK_PRIORITY = 2`), `crates/harness-server/src/task_runner/state.rs:311-314`
  (`pub priority: u8`, 0=normal 1=high 2=critical), validated at
  `crates/harness-server/src/services/execution.rs:367-371`, passed to
  `queue.acquire(...)` at `execution.rs:1095` and `execution.rs:1193`.
  DB column: migration v12 at
  `crates/harness-server/src/task_db/migrations.rs:101-102`.
- Stats consumers: `crates/harness-server/src/handlers/dashboard.rs:165`,
  `crates/harness-server/src/handlers/dashboard_active_counts.rs:169`,
  `crates/harness-server/src/http/misc_routes.rs:118`, and admission-failure
  logging via `diagnostics()` at
  `crates/harness-server/src/services/execution.rs:249`.

## Proposed Design

### Aging function

```text
waited = now.saturating_duration_since(w.enqueued_at)
effective_priority(w, now) =
    min(w.priority + floor(waited / interval),
        min(w.priority + max_boost_levels, MAX_LEVEL))
```

- `MAX_LEVEL` is a static ceiling fixed at queue construction (the server
  passes `MAX_TASK_PRIORITY`, `request.rs:9`; the queue stays generic over
  u8). It is deliberately NOT derived from currently-enqueued waiters: a
  dynamic observed ceiling would decrease when a high-priority waiter is
  granted and leaves the queue, making effective priority non-monotonic
  over wait time and violating B-001.
- `saturating_duration_since` guards the `Instant` subtraction: a waiter
  whose `enqueued_at` races ahead of the single `now` read in `release()`
  (concurrent enqueue, paused-clock test anomalies) contributes zero wait
  instead of panicking on `Instant` underflow.
- Ordering key: `(effective_priority DESC, seq ASC)`. Aged-equal ties break
  by seq, so a long-waiting priority-0 waiter beats a fresh same-level
  arrival deterministically (B-004).

### Data-structure change: BinaryHeap -> Vec with lazy scan

`BinaryHeap` ordering is fixed at push time, but aging makes the key
time-varying, silently invalidating the heap invariant. Replace
`waiters: BinaryHeap<Waiter>` with `Vec<Waiter>` and select the winner in
`release()` via a linear max-scan using `effective_priority(now)`.
`waiters.len()` is bounded by `max_queue_size` (default 32,
`misc.rs:164-166`), so O(n) per release is negligible against permit
lifetimes (task executions). `drain_cancelled()` becomes `Vec::retain`
(same semantics). With aging disabled, the scan key is exactly
`(priority DESC, seq ASC)` — the current comparator — so grant order is
unchanged (B-003).

### Clock

`Waiter` gains `enqueued_at: tokio::time::Instant` captured in
`try_acquire_or_enqueue`. `tokio::time::Instant` is monotonic and obeys the
paused test clock (`start_paused = true`), giving deterministic aging tests
via `tokio::time::advance` (B-005). `release()` reads `Instant::now()` once
per call and passes it to the comparator. No `std::time::SystemTime`
anywhere in the queue.

Note: `release()` is also called from `Drop` impls
(`task_queue.rs:58-64, 336-402`); `tokio::time::Instant::now()` is a plain
read, safe outside an async context as long as a runtime created the
instants — same constraint the tests already satisfy.

### Wait-time metrics

- `PriorityPermitQueue` gains a small per-base-priority-level aggregate:
  grant count, max wait, and a fixed ring buffer (e.g. 128 samples) for p95.
  Recording happens on the waiter side, after `rx.await` resolves and the
  grant is actually consumed — not in `release()` at `tx.send` time. This
  closes the mid-send race: if a waiter is cancelled after `tx.send`
  succeeds but before `rx.await` completes, the permit is CAS-reclaimed by
  `AcquireGuard::drop` and no sample is ever recorded for that waiter, so
  cancelled and reclaimed grants are structurally excluded (B-006) rather
  than filtered after the fact. The waiter records via the shared queue
  handle it already holds.
- `QueueStats` gains `wait_ms_by_priority: Vec<PriorityWaitStats>` with
  `#[serde(default)]`-safe additive fields (`QueueStats` derives
  `Serialize` only, so consumers at `dashboard.rs:165`,
  `dashboard_active_counts.rs:169`, `misc_routes.rs:118` get the new field
  additively). `QueueDiagnostics` gains the same for the global queue,
  surfacing in admission-failure logs (`execution.rs:249`).

### Config (harness-core)

New struct in `crates/harness-core/src/config/misc.rs`, nested in
`ConcurrencyConfig` following the existing serde-default pattern:

```toml
[concurrency.aging]
enabled = true          # default ON
interval_secs = 300     # +1 effective level per 300s waited (conservative)
max_boost_levels = 2    # cap: base + 2, never above max level
```

`ConcurrencyConfig` gains `#[serde(default)] pub aging: PriorityAgingConfig`
so existing config files load unchanged (B-009). Config validation rejects
`enabled = true` with `interval_secs = 0` at load with an error (B-010) —
no silent clamp (U-29). `TaskQueue::new` threads the aging parameters into
each `PriorityPermitQueue` it creates (project queues created lazily at
`task_queue.rs:497-505` receive the same parameters).

## Edge Cases

- **Aging + cancellation**: a cancelled waiter is skipped by the scan
  regardless of aged priority; `drain_cancelled` unchanged in effect. A
  caller that retries `acquire` after cancel gets a fresh `enqueued_at`
  (no wait-time carryover) — documented, intentional.
- **Aging + per-project vs global**: the two stages age independently;
  end-to-end bound = sum of stage bounds (B-007). Documented on
  `TaskQueue::acquire`.
- **Ties after aging**: strictly `(effective DESC, seq ASC)`; no randomness.
- **`reconfigure_capacity` / `set_project_limit`**: orthogonal — aging
  parameters are per-queue constants, capacity changes untouched.
- **Boost overflow**: u8 saturating arithmetic; cap applied before compare.

## Compatibility

- `aging.enabled = false` short-circuits `effective_priority` to the base
  priority; the Vec max-scan with key `(priority DESC, seq ASC)` selects the
  same waiter the BinaryHeap pop selects today for every sequence (B-003).
- All existing tests in `task_queue_tests.rs` must pass without
  modification (aging defaults do not alter any existing test because those
  tests never advance the paused clock past `interval_secs`).
- Additive `Serialize` fields only; no field removed or renamed on
  `QueueStats`/`QueueDiagnostics` (dashboard JSON stays a superset).

## Product-to-Test Mapping

| Invariant | Implementation area | Verification |
|---|---|---|
| B-001 | `effective_priority` fn, `task_queue.rs` | `cargo test -p harness-workflow task_queue` (unit test: boost per interval, cap) |
| B-002 | `release()` scan + paused clock | `cargo test -p harness-workflow task_queue` (test: `aged_low_priority_not_starved_beyond_bound`) |
| B-003 | scan comparator with aging off | `cargo test -p harness-workflow task_queue` (existing `high_priority_acquired_before_low_when_slots_full`, `same_priority_is_fifo` unchanged + new `aging_off_matches_legacy_order`) |
| B-004 | tie-break by seq | `cargo test -p harness-workflow task_queue` (test: aged tie resolves to earlier seq) |
| B-005 | `tokio::time::Instant` only | `cargo test -p harness-workflow task_queue` (all aging tests use `start_paused`) + `rg -n "SystemTime|Utc::now" crates/harness-workflow/src/task_queue.rs` returns nothing |
| B-006 | cancelled-skip in scan, metrics exclusion | `cargo test -p harness-workflow task_queue` (extend `cancelled_project_wait_does_not_leak_queued_count` family with aging on) |
| B-007 | two-stage acquire docs + test | `cargo test -p harness-workflow task_queue` (test: bound holds per stage with both queues contended) |
| B-008 | `QueueStats`/`QueueDiagnostics` fields | `cargo test -p harness-workflow task_queue` (stats assertions) + `cargo check -p harness-server` (consumers compile) |
| B-009 | `PriorityAgingConfig` serde defaults | `cargo test -p harness-core config` (empty-TOML deserialization test, mirrors `misc.rs:770`) |
| B-010 | config load validation | `cargo test -p harness-core config` (invalid `interval_secs = 0` returns error) |

## Verification Plan

- `cargo test -p harness-workflow task_queue` — full queue suite (existing
  24 tests + new aging tests), all deterministic via paused clock, no
  wall-clock sleeps.
- `cargo test -p harness-core config` — config defaults + validation.
- `cargo check --workspace` then
  `cargo clippy --workspace --all-targets -- -D warnings` (CI parity).

## Rollback

- Operator rollback: set `[concurrency.aging] enabled = false` — restores
  current ordering semantics exactly with no redeploy of data (no schema or
  DB change in this issue).
- Code rollback: single-PR revert; the only cross-crate surface is the
  additive config struct and additive stats fields, so a revert cannot
  break config files or dashboard consumers.
