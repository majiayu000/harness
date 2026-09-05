//! Single-stage priority permit queue with aging-based anti-starvation.
//!
//! Internal engine behind [`super::TaskQueue`]: each stage (project, global)
//! owns one `PriorityPermitQueue`.

use harness_core::config::misc::PriorityAgingConfig;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

/// Maximum effective priority level.
///
/// Mirrors `MAX_TASK_PRIORITY` in harness-server (`task_runner/request.rs`):
/// 0 = normal, 1 = high, 2 = critical. This is a static construction-time
/// ceiling for priority aging — it is deliberately NOT derived from
/// currently-enqueued waiters, because a dynamic observed ceiling would
/// decrease when a high-priority waiter leaves the queue, making effective
/// priority non-monotonic over wait time.
pub const MAX_PRIORITY_LEVEL: u8 = 2;

/// Number of wait-time samples retained per priority level for p95 estimation.
const WAIT_SAMPLE_RING_SIZE: usize = 128;

/// Components returned when a caller is enqueued instead of immediately granted a permit.
/// Contains the notification receiver, the permit-granted flag, the cancellation
/// flag, and the enqueue timestamp (for waiter-side wait-time metrics).
pub(crate) type EnqueuedWaiter = (
    oneshot::Receiver<()>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Instant,
);

/// Wait-time aggregates for granted waiters at one base priority level.
///
/// Samples are recorded waiter-side after the grant is actually consumed
/// (`rx.await` resolved), so cancelled or reclaimed grants never contribute.
#[derive(Debug, Clone, Serialize)]
pub struct PriorityWaitStats {
    /// Base priority level of the granted waiters (aging boosts are not reflected here).
    pub priority: u8,
    /// Number of waiters granted a permit after queueing at this level.
    pub granted: u64,
    /// Maximum observed wait in milliseconds.
    pub max_wait_ms: u64,
    /// p95 wait in milliseconds over a ring buffer of recent samples.
    pub p95_wait_ms: u64,
}

/// Zeroed wait-time snapshot for projects whose queue has not been created yet.
pub(crate) fn empty_wait_stats() -> Vec<PriorityWaitStats> {
    (0..=MAX_PRIORITY_LEVEL)
        .map(|priority| PriorityWaitStats {
            priority,
            granted: 0,
            max_wait_ms: 0,
            p95_wait_ms: 0,
        })
        .collect()
}

/// An entry waiting for a permit. `release()` selects the waiter with the
/// highest effective priority (base priority plus aging boost), breaking ties
/// by enqueue sequence (earlier waiter wins).
struct Waiter {
    priority: u8,
    /// Monotonically increasing sequence number; lower seq = earlier enqueue =
    /// higher rank among equal-effective-priority waiters (FIFO within level).
    seq: u64,
    /// When this waiter was enqueued. Uses `tokio::time::Instant` so aging is
    /// monotonic and obeys the paused test clock — wall-clock adjustments can
    /// never reorder waiters.
    enqueued_at: Instant,
    tx: oneshot::Sender<()>,
    /// Set to `true` by `release()` before calling `tx.send()`.
    ///
    /// Shared with the waiting `AcquireGuard` so that Drop can detect the
    /// "permit granted but future cancelled before rx.await completed" race
    /// and return the permit to the pool via a CAS operation.
    permit_granted: Arc<AtomicBool>,
    /// Set to `true` by `AcquireGuard::Drop` when the future is cancelled
    /// while this waiter is still sitting in the queue (before `release()` has
    /// selected and signalled it).  Allows `release()` to skip stale entries
    /// cheaply without attempting a channel send that is guaranteed to fail.
    cancelled: Arc<AtomicBool>,
}

/// Aging parameters fixed at queue construction.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AgingParams {
    enabled: bool,
    /// Wait duration per one effective-priority boost level.
    interval: Duration,
    /// Maximum boost above the base priority.
    max_boost_levels: u8,
    /// Static ceiling for effective priority (never derived from enqueued waiters).
    max_level: u8,
}

impl AgingParams {
    pub(crate) fn from_config(cfg: &PriorityAgingConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            interval: Duration::from_secs(cfg.interval_secs),
            max_boost_levels: cfg.max_boost_levels,
            max_level: MAX_PRIORITY_LEVEL,
        }
    }
}

/// Per-priority-level wait-time aggregate backing [`PriorityWaitStats`].
#[derive(Debug, Default)]
struct LevelWaitAgg {
    granted: u64,
    max_wait_ms: u64,
    /// Fixed-size ring buffer of recent wait samples (milliseconds).
    samples: Vec<u64>,
    next_sample: usize,
}

impl LevelWaitAgg {
    fn record(&mut self, wait_ms: u64) {
        self.granted += 1;
        self.max_wait_ms = self.max_wait_ms.max(wait_ms);
        if self.samples.len() < WAIT_SAMPLE_RING_SIZE {
            self.samples.push(wait_ms);
        } else {
            self.samples[self.next_sample] = wait_ms;
            self.next_sample = (self.next_sample + 1) % WAIT_SAMPLE_RING_SIZE;
        }
    }

    fn p95_ms(&self) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let idx = (sorted.len() * 95).div_ceil(100).saturating_sub(1);
        sorted[idx]
    }
}

/// Priority-aware permit queue with aging-based anti-starvation.
///
/// Replaces `tokio::sync::Semaphore` so that high-priority waiters can skip
/// ahead of lower-priority ones when a slot becomes free.  With aging enabled,
/// a waiter's effective priority rises one level per `interval` waited, capped
/// at `base + max_boost_levels` and never above `max_level`, so a continuous
/// stream of high-priority arrivals cannot starve priority-0 waiters beyond a
/// deterministic bound.  With aging disabled, selection is exactly
/// `(priority DESC, seq ASC)` — the pre-aging ordering.
///
/// Waiters live in a `Vec` and the winner is chosen by a lazy max-scan in
/// `release()`: the aging key is time-varying, which would silently invalidate
/// a `BinaryHeap`'s push-time ordering.  `waiters.len()` is bounded by
/// `max_queue_size`, so the O(n) scan is negligible against permit lifetimes.
///
/// # Known limitations
/// - **Priority inversion**: a low-priority task *holding* a permit blocks a
///   high-priority waiter.  Aging only affects waiters, not preemption.
pub(crate) struct PriorityPermitQueue {
    available: usize,
    next_seq: u64,
    waiters: Vec<Waiter>,
    aging: AgingParams,
    /// Wait-time aggregates indexed by base priority level (0..=max_level).
    wait_stats: Vec<LevelWaitAgg>,
}

impl PriorityPermitQueue {
    pub(crate) fn new(capacity: usize, aging: AgingParams) -> Self {
        let levels = usize::from(aging.max_level) + 1;
        Self {
            available: capacity,
            next_seq: 0,
            waiters: Vec::new(),
            aging,
            wait_stats: (0..levels).map(|_| LevelWaitAgg::default()).collect(),
        }
    }

    /// Attempt to claim a permit immediately.
    ///
    /// Returns `Ok(())` when a slot is free.  Otherwise pushes a `Waiter` and
    /// returns the `Receiver` half of the notification channel, the shared
    /// `permit_granted` flag, the shared `cancelled` flag, and the enqueue
    /// timestamp; the caller **must** `.await` the receiver **outside** this lock.
    pub(crate) fn try_acquire_or_enqueue(&mut self, priority: u8) -> Result<(), EnqueuedWaiter> {
        if self.available > 0 {
            self.available -= 1;
            Ok(())
        } else {
            let permit_granted = Arc::new(AtomicBool::new(false));
            let cancelled = Arc::new(AtomicBool::new(false));
            let (tx, rx) = oneshot::channel();
            let seq = self.next_seq;
            self.next_seq += 1;
            let enqueued_at = Instant::now();
            self.waiters.push(Waiter {
                priority,
                seq,
                enqueued_at,
                tx,
                permit_granted: permit_granted.clone(),
                cancelled: cancelled.clone(),
            });
            Err((rx, permit_granted, cancelled, enqueued_at))
        }
    }

    /// Effective priority of a waiter at `now`.
    ///
    /// With aging disabled this is the base priority.  With aging enabled the
    /// base is boosted one level per `interval` waited, capped at
    /// `base + max_boost_levels` and never above `max_level`; the result is
    /// monotonically non-decreasing over wait time and never below the base.
    /// `saturating_duration_since` guards against a waiter whose `enqueued_at`
    /// races ahead of the single `now` read in `release()`.
    fn effective_priority(&self, waiter: &Waiter, now: Instant) -> u8 {
        if !self.aging.enabled {
            return waiter.priority;
        }
        let waited = now.saturating_duration_since(waiter.enqueued_at);
        let boosts = waited
            .as_millis()
            .checked_div(self.aging.interval.as_millis())
            .unwrap_or(0);
        let boost = boosts.min(u128::from(self.aging.max_boost_levels)) as u8;
        let capped = waiter
            .priority
            .saturating_add(boost)
            .min(self.aging.max_level);
        // A base priority above max_level is never lowered by aging.
        capped.max(waiter.priority)
    }

    /// Index of the next waiter to grant: highest effective priority at `now`,
    /// ties broken by enqueue sequence (earlier waiter wins).  Cancelled
    /// waiters are skipped.  Returns `None` when no live waiter exists.
    fn select_waiter_index(&self, now: Instant) -> Option<usize> {
        let mut best: Option<(usize, u8, u64)> = None;
        for (idx, waiter) in self.waiters.iter().enumerate() {
            if waiter.cancelled.load(AtomicOrdering::SeqCst) {
                continue;
            }
            let eff = self.effective_priority(waiter, now);
            let better = match best {
                None => true,
                Some((_, best_eff, best_seq)) => {
                    eff > best_eff || (eff == best_eff && waiter.seq < best_seq)
                }
            };
            if better {
                best = Some((idx, eff, waiter.seq));
            }
        }
        best.map(|(idx, _, _)| idx)
    }

    /// Release a permit: wake the best-ranked waiter, or increment `available`.
    ///
    /// Sets `permit_granted` on the waiter **before** sending so that the
    /// waiter's `AcquireGuard::Drop` can detect the window where the permit
    /// signal was already sent but the future was cancelled before `rx.await`
    /// could complete.  A CAS is used to avoid double-release if both this
    /// function and the guard's Drop try to reclaim the permit concurrently.
    pub(crate) fn release(&mut self) {
        // Single time read per release so one scan uses one consistent `now`.
        let now = Instant::now();
        loop {
            let Some(idx) = self.select_waiter_index(now) else {
                // No live waiter; drop any cancelled leftovers and free the slot.
                self.drain_cancelled();
                self.available += 1;
                return;
            };
            let waiter = self.waiters.swap_remove(idx);
            // Mark granted BEFORE sending so the guard's Drop can see
            // it even if the future is cancelled between store and send.
            waiter.permit_granted.store(true, AtomicOrdering::SeqCst);
            if waiter.tx.send(()).is_ok() {
                // Permit is in transit to the waiter. If the waiter
                // cancels before rx.await completes, its Drop handler
                // uses CAS to reclaim and re-release the permit.
                return;
            }
            // tx.send() failed: rx was already dropped.
            // Try to reclaim via CAS before looping to the next waiter.
            // If the guard's Drop already ran and reclaimed (CAS fails),
            // it already called release() — return to avoid double-release.
            if waiter
                .permit_granted
                .compare_exchange(true, false, AtomicOrdering::SeqCst, AtomicOrdering::SeqCst)
                .is_err()
            {
                // Guard's Drop won the race and called release() already.
                return;
            }
            // We reclaimed; continue to grant the permit to the next waiter.
        }
    }

    /// Remove all waiters that have been marked cancelled.
    ///
    /// Called from `AcquireGuard::Drop` when a queue-resident waiter is
    /// cancelled to prevent stale entries from accumulating unboundedly
    /// under repeated enqueue/cancel patterns.  Without eager removal the
    /// queue can grow to `max_queue_size × N` entries after N cycles because
    /// `queued_count` (the admission gate) decrements on cancel while the
    /// queue entry remains.
    pub(crate) fn drain_cancelled(&mut self) {
        self.waiters
            .retain(|w| !w.cancelled.load(AtomicOrdering::SeqCst));
    }

    /// Record a wait-time sample for a granted waiter at its base priority.
    ///
    /// Called waiter-side after `rx.await` resolves and the grant is actually
    /// consumed — never from `release()` at `tx.send` time — so cancelled and
    /// CAS-reclaimed grants are structurally excluded from the metrics.
    pub(crate) fn record_granted_wait(&mut self, priority: u8, waited: Duration) {
        let level = usize::from(priority.min(self.aging.max_level));
        let wait_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX);
        self.wait_stats[level].record(wait_ms);
    }

    pub(crate) fn wait_stats_snapshot(&self) -> Vec<PriorityWaitStats> {
        self.wait_stats
            .iter()
            .enumerate()
            .map(|(level, agg)| PriorityWaitStats {
                priority: level as u8,
                granted: agg.granted,
                max_wait_ms: agg.max_wait_ms,
                p95_wait_ms: agg.p95_ms(),
            })
            .collect()
    }

    pub(crate) fn available_permits(&self) -> usize {
        self.available
    }

    pub(crate) fn waiter_count(&self) -> usize {
        self.waiters.len()
    }

    /// Reconfigure queue capacity while preserving already-issued permits.
    pub(crate) fn reconfigure_capacity(&mut self, old_capacity: usize, new_capacity: usize) {
        let issued = old_capacity.saturating_sub(self.available);
        self.available = new_capacity.saturating_sub(issued);
    }

    /// Test-only view: effective priority of the queued waiter at `index`
    /// (enqueue order) as computed at `now`.
    #[cfg(test)]
    pub(crate) fn effective_priority_at(&self, index: usize, now: Instant) -> u8 {
        self.effective_priority(&self.waiters[index], now)
    }
}
