use dashmap::DashMap;
use harness_core::config::misc::ConcurrencyConfig;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Per-project and global queue statistics.
#[derive(Debug, Clone, Serialize)]
pub struct QueueStats {
    /// Number of tasks currently holding a project execution slot.
    pub running: usize,
    /// Number of tasks waiting for a project execution slot.
    pub queued: usize,
    /// Maximum concurrent tasks for this project (or globally).
    pub limit: usize,
}

/// Acquired execution slot. Dropping this releases both the global and project slots.
pub struct TaskPermit {
    _global_release: PermitReleaseHandle,
    _project_release: PermitReleaseHandle,
}

impl std::fmt::Debug for TaskPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskPermit").finish()
    }
}

/// RAII guard that calls `PriorityPermitQueue::release()` on drop.
///
/// Uses `std::sync::Mutex` (not tokio) so the critical section (heap push/pop)
/// can be taken synchronously inside `Drop`.  The critical section is
/// microseconds — blocking the thread briefly is acceptable.
struct PermitReleaseHandle {
    queue: Arc<Mutex<PriorityPermitQueue>>,
}

impl Drop for PermitReleaseHandle {
    fn drop(&mut self) {
        // Mutex::lock().unwrap() is the right pattern here (RS-03 exemption:
        // poisoned-lock panic is correct behaviour).
        self.queue.lock().unwrap().release();
    }
}

/// An entry waiting for a permit, ordered by (priority DESC, seq ASC) so
/// that the `BinaryHeap` max-heap yields the highest-priority / earliest waiter.
struct Waiter {
    priority: u8,
    /// Monotonically increasing sequence number; lower seq = earlier enqueue =
    /// higher priority among equal-priority waiters (FIFO within same level).
    seq: u64,
    tx: oneshot::Sender<()>,
}

impl PartialEq for Waiter {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}

impl Eq for Waiter {}

impl PartialOrd for Waiter {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Waiter {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority wins.  Tie-break: lower seq → higher rank (FIFO).
        match self.priority.cmp(&other.priority) {
            Ordering::Equal => other.seq.cmp(&self.seq),
            ord => ord,
        }
    }
}

/// Priority-aware permit queue.
///
/// Replaces `tokio::sync::Semaphore` so that high-priority waiters can skip
/// ahead of lower-priority ones when a slot becomes free.
///
/// # Known limitations
/// - **Priority inversion**: a low-priority task holding a permit blocks a
///   high-priority waiter.  Full aging/anti-starvation is a follow-up.
/// - **Low-priority starvation**: a continuous stream of high-priority tasks
///   can starve priority-0 waiters indefinitely.  Callers are responsible for
///   bounding this at a higher level.
struct PriorityPermitQueue {
    available: usize,
    next_seq: u64,
    waiters: BinaryHeap<Waiter>,
}

impl PriorityPermitQueue {
    fn new(capacity: usize) -> Self {
        Self {
            available: capacity,
            next_seq: 0,
            waiters: BinaryHeap::new(),
        }
    }

    /// Attempt to claim a permit immediately.
    ///
    /// Returns `Ok(())` when a slot is free.  Otherwise pushes a `Waiter` and
    /// returns the `Receiver` half of the notification channel; the caller
    /// **must** `.await` it **outside** this lock.
    fn try_acquire_or_enqueue(&mut self, priority: u8) -> Result<(), oneshot::Receiver<()>> {
        if self.available > 0 {
            self.available -= 1;
            Ok(())
        } else {
            let (tx, rx) = oneshot::channel();
            let seq = self.next_seq;
            self.next_seq += 1;
            self.waiters.push(Waiter { priority, seq, tx });
            Err(rx)
        }
    }

    /// Release a permit: wake the top-priority waiter, or increment `available`.
    fn release(&mut self) {
        loop {
            match self.waiters.pop() {
                None => {
                    self.available += 1;
                    return;
                }
                Some(waiter) => {
                    // Receiver already dropped means the waiter was cancelled.
                    // Skip it and try the next one.
                    if waiter.tx.send(()).is_ok() {
                        return;
                    }
                }
            }
        }
    }

    fn available_permits(&self) -> usize {
        self.available
    }

    fn waiter_count(&self) -> usize {
        self.waiters.len()
    }
}

/// Bounded task queue with priority-aware scheduling.
///
/// Maintains a global `PriorityPermitQueue` limiting total concurrent tasks
/// across all projects, plus per-project queues so one project cannot starve
/// others.  Acquiring a slot requires holding BOTH permits simultaneously.
///
/// Higher `priority` values are served first when multiple tasks are waiting.
/// Within the same priority level, tasks are served FIFO.
pub struct TaskQueue {
    global_queue: Arc<Mutex<PriorityPermitQueue>>,
    global_limit: usize,
    max_queue_size: usize,
    queued_count: AtomicUsize,
    /// Per-project execution queues. Created on first use.
    project_queues: DashMap<String, Arc<Mutex<PriorityPermitQueue>>>,
    /// Configured per-project max concurrent limits. Missing entries fall back to `global_limit`.
    project_limits: DashMap<String, usize>,
    /// Per-project count of tasks waiting for a project-level slot.
    project_queued: DashMap<String, Arc<AtomicUsize>>,
    /// Per-project count of tasks that hold the project slot but are still
    /// waiting for the global slot. These are "queued" not "running" from an
    /// observability standpoint — they haven't started executing yet.
    project_awaiting_global: DashMap<String, Arc<AtomicUsize>>,
    /// Optional memory-pressure flag set by [`crate::memory_monitor`].
    /// When `true`, `acquire()` rejects new tasks immediately.
    /// `None` means the feature is disabled (default).
    memory_pressure: Option<Arc<AtomicBool>>,
}

impl TaskQueue {
    pub fn new(config: &ConcurrencyConfig) -> Self {
        Self::new_with_pressure(config, None)
    }

    /// Like [`TaskQueue::new`] but wires in an optional memory-pressure flag.
    /// When the flag is `Some` and its value is `true`, `acquire()` returns an
    /// error immediately instead of waiting for a semaphore slot.
    pub fn new_with_pressure(
        config: &ConcurrencyConfig,
        memory_pressure: Option<Arc<AtomicBool>>,
    ) -> Self {
        let project_limits: DashMap<String, usize> = config
            .per_project
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        Self {
            global_queue: Arc::new(Mutex::new(PriorityPermitQueue::new(
                config.max_concurrent_tasks,
            ))),
            global_limit: config.max_concurrent_tasks,
            max_queue_size: config.max_queue_size,
            queued_count: AtomicUsize::new(0),
            project_queues: DashMap::new(),
            project_limits,
            project_queued: DashMap::new(),
            project_awaiting_global: DashMap::new(),
            memory_pressure,
        }
    }

    fn project_limit(&self, project_id: &str) -> usize {
        self.project_limits
            .get(project_id)
            .map(|v| *v)
            .unwrap_or(self.global_limit)
    }

    fn get_or_create_project_queue(&self, project_id: &str) -> Arc<Mutex<PriorityPermitQueue>> {
        self.project_queues
            .entry(project_id.to_string())
            .or_insert_with(|| {
                let limit = self.project_limit(project_id);
                Arc::new(Mutex::new(PriorityPermitQueue::new(limit)))
            })
            .clone()
    }

    fn get_or_create_project_queued(&self, project_id: &str) -> Arc<AtomicUsize> {
        self.project_queued
            .entry(project_id.to_string())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone()
    }

    fn get_or_create_project_awaiting_global(&self, project_id: &str) -> Arc<AtomicUsize> {
        self.project_awaiting_global
            .entry(project_id.to_string())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone()
    }

    /// Acquire a global + project-level execution slot, respecting `priority`.
    ///
    /// Higher `priority` values are served before lower values when multiple
    /// tasks are waiting. Within the same priority level, tasks are served FIFO.
    ///
    /// Acquiring a project permit first prevents a single project from
    /// monopolizing all global slots while waiting for its own limit.
    ///
    /// Returns `Err` immediately if `max_queue_size` tasks are already waiting,
    /// or if the memory-pressure flag (set by `memory_monitor`) is `true`.
    /// The returned permit releases both slots on drop (even on panic).
    pub async fn acquire(&self, project_id: &str, priority: u8) -> anyhow::Result<TaskPermit> {
        // Reserve a global queue slot. fetch_add returns value before increment,
        // so if prev == max_queue_size the queue is already full.
        let prev = self.queued_count.fetch_add(1, AtomicOrdering::SeqCst);
        if prev >= self.max_queue_size {
            self.queued_count.fetch_sub(1, AtomicOrdering::SeqCst);
            return Err(anyhow::anyhow!(
                "task queue is full (max_queue_size={})",
                self.max_queue_size
            ));
        }

        let project_queue = self.get_or_create_project_queue(project_id);
        let project_queued_counter = self.get_or_create_project_queued(project_id);
        project_queued_counter.fetch_add(1, AtomicOrdering::SeqCst);

        // Acquire the project-level slot first. This ensures a project cannot
        // hold global slots while waiting for its own concurrency limit,
        // preventing one project from starving others.
        //
        // Lock briefly to either claim a slot or register a waiter, then release
        // the std::sync::Mutex before awaiting so we never hold it across .await.
        let project_rx = project_queue
            .lock()
            .unwrap()
            .try_acquire_or_enqueue(priority);
        if let Err(rx) = project_rx {
            if rx.await.is_err() {
                self.queued_count.fetch_sub(1, AtomicOrdering::SeqCst);
                project_queued_counter.fetch_sub(1, AtomicOrdering::SeqCst);
                return Err(anyhow::anyhow!("project task queue closed"));
            }
        }

        // Project slot acquired; transition from "waiting for project" to
        // "waiting for global". Keep this task counted as queued (not running)
        // until it also holds the global slot.
        project_queued_counter.fetch_sub(1, AtomicOrdering::SeqCst);
        let project_awaiting_counter = self.get_or_create_project_awaiting_global(project_id);
        project_awaiting_counter.fetch_add(1, AtomicOrdering::SeqCst);

        // Then acquire the global slot.
        let global_rx = self
            .global_queue
            .lock()
            .unwrap()
            .try_acquire_or_enqueue(priority);
        if let Err(rx) = global_rx {
            if rx.await.is_err() {
                // Global queue was dropped. Release the project permit we hold.
                project_queue.lock().unwrap().release();
                self.queued_count.fetch_sub(1, AtomicOrdering::SeqCst);
                project_awaiting_counter.fetch_sub(1, AtomicOrdering::SeqCst);
                return Err(anyhow::anyhow!("task queue closed"));
            }
        }

        project_awaiting_counter.fetch_sub(1, AtomicOrdering::SeqCst);
        self.queued_count.fetch_sub(1, AtomicOrdering::SeqCst);

        Ok(TaskPermit {
            _global_release: PermitReleaseHandle {
                queue: self.global_queue.clone(),
            },
            _project_release: PermitReleaseHandle {
                queue: project_queue,
            },
        })
    }

    /// Number of tasks currently holding a global execution slot (running).
    pub fn running_count(&self) -> usize {
        let q = self.global_queue.lock().unwrap();
        self.global_limit
            .saturating_sub(q.available_permits())
            .saturating_sub(q.waiter_count())
    }

    /// Number of tasks waiting for a global execution slot.
    pub fn queued_count(&self) -> usize {
        self.queued_count.load(AtomicOrdering::Relaxed)
    }

    /// Global execution limit.
    pub fn global_limit(&self) -> usize {
        self.global_limit
    }

    /// Stats for a specific project (tasks that have acquired a project slot).
    pub fn project_stats(&self, project_id: &str) -> QueueStats {
        let limit = self.project_limit(project_id);
        if let Some(pq) = self.project_queues.get(project_id) {
            let q = pq.lock().unwrap();
            let holding_project = limit.saturating_sub(q.available_permits());
            let waiting_for_project = q.waiter_count();
            drop(q);
            let awaiting_global = self
                .project_awaiting_global
                .get(project_id)
                .map(|c| c.load(AtomicOrdering::Relaxed))
                .unwrap_or(0);
            let running = holding_project.saturating_sub(awaiting_global);
            let queued = waiting_for_project + awaiting_global;
            QueueStats {
                running,
                queued,
                limit,
            }
        } else {
            QueueStats {
                running: 0,
                queued: 0,
                limit,
            }
        }
    }

    /// Stats for all projects that have been used at least once.
    pub fn all_project_stats(&self) -> Vec<(String, QueueStats)> {
        self.project_queues
            .iter()
            .map(|entry| {
                let project_id = entry.key().clone();
                let stats = self.project_stats(&project_id);
                (project_id, stats)
            })
            .collect()
    }

    /// Create an unbounded queue for tests (effectively no limit).
    #[cfg(test)]
    pub fn unbounded() -> Self {
        Self::new(&ConcurrencyConfig {
            max_concurrent_tasks: 1024,
            max_queue_size: 1024,
            stall_timeout_secs: 300,
            per_project: Default::default(),
            ..ConcurrencyConfig::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn config(max_concurrent: usize, max_queue: usize) -> ConcurrencyConfig {
        ConcurrencyConfig {
            max_concurrent_tasks: max_concurrent,
            max_queue_size: max_queue,
            stall_timeout_secs: 300,
            per_project: Default::default(),
            ..ConcurrencyConfig::default()
        }
    }

    #[tokio::test]
    async fn acquire_and_release_single_permit() {
        let q = TaskQueue::new(&config(2, 8));
        assert_eq!(q.running_count(), 0);
        let permit = q.acquire("proj", 0).await.unwrap();
        assert_eq!(q.running_count(), 1);
        drop(permit);
        assert_eq!(q.running_count(), 0);
    }

    #[tokio::test]
    async fn only_max_concurrent_tasks_run_simultaneously() {
        let q = Arc::new(TaskQueue::new(&config(2, 16)));

        // Acquire both permits.
        let p1 = q.acquire("proj", 0).await.unwrap();
        let p2 = q.acquire("proj", 0).await.unwrap();
        assert_eq!(q.running_count(), 2);

        // Third acquire must block; verify with a short timeout.
        let q2 = q.clone();
        let blocked = timeout(Duration::from_millis(50), q2.acquire("proj", 0)).await;
        assert!(blocked.is_err(), "third acquire should block when limit=2");

        // Release a slot; third acquire should now succeed.
        drop(p1);
        let p3 = q.acquire("proj", 0).await.unwrap();
        assert_eq!(q.running_count(), 2);
        drop(p2);
        drop(p3);
        assert_eq!(q.running_count(), 0);
    }

    #[tokio::test]
    async fn queue_overflow_returns_error() {
        // 1 concurrent slot, queue capacity 2.
        let q = Arc::new(TaskQueue::new(&config(1, 2)));

        // Hold the single execution slot.
        let _p1 = q.acquire("proj", 0).await.unwrap();

        // Two tasks queue up (they block, so spawn them).
        let q2 = q.clone();
        let _h1 = tokio::spawn(async move { q2.acquire("proj", 0).await });
        let q3 = q.clone();
        let _h2 = tokio::spawn(async move { q3.acquire("proj", 0).await });

        // Give spawned tasks time to increment queued_count.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Third waiter exceeds max_queue_size=2 → error.
        let result = q.acquire("proj", 0).await;
        assert!(result.is_err(), "expected queue full error");
        assert!(result.unwrap_err().to_string().contains("max_queue_size=2"));
    }

    #[tokio::test]
    async fn permit_drop_releases_slot_on_panic() {
        let q = Arc::new(TaskQueue::new(&config(1, 4)));

        {
            let q_inner = q.clone();
            // Run in a separate task that acquires a permit then panics.
            let handle = tokio::spawn(async move {
                let _permit = q_inner.acquire("proj", 0).await.unwrap();
                panic!("forced panic to test permit drop");
            });
            let _ = handle.await; // ignore the JoinError from the panic
        }

        // The permit must have been released; acquiring again should succeed immediately.
        let result = timeout(Duration::from_millis(100), q.acquire("proj", 0)).await;
        assert!(result.is_ok(), "permit should be released after panic");
    }

    #[tokio::test]
    async fn queued_count_increments_while_waiting() {
        let q = Arc::new(TaskQueue::new(&config(1, 8)));

        // Hold the slot so the next acquire must wait.
        let _holder = q.acquire("proj", 0).await.unwrap();
        assert_eq!(q.queued_count(), 0);

        let q2 = q.clone();
        let _waiter = tokio::spawn(async move { q2.acquire("proj", 0).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(q.queued_count(), 1);
    }

    #[tokio::test]
    async fn running_count_and_queued_count_reset_after_completion() {
        let q = TaskQueue::new(&config(4, 16));
        let p1 = q.acquire("proj", 0).await.unwrap();
        let p2 = q.acquire("proj", 0).await.unwrap();
        assert_eq!(q.running_count(), 2);
        drop(p1);
        drop(p2);
        assert_eq!(q.running_count(), 0);
        assert_eq!(q.queued_count(), 0);
    }

    #[tokio::test]
    async fn per_project_limit_enforced() {
        use std::collections::HashMap;
        let mut per_project = HashMap::new();
        per_project.insert("proj_a".to_string(), 1usize);
        let cfg = ConcurrencyConfig {
            max_concurrent_tasks: 4,
            max_queue_size: 16,
            stall_timeout_secs: 300,
            per_project,
            ..ConcurrencyConfig::default()
        };
        let q = Arc::new(TaskQueue::new(&cfg));

        // proj_a has limit=1; second acquire must block.
        let _p1 = q.acquire("proj_a", 0).await.unwrap();
        let q2 = q.clone();
        let blocked = timeout(Duration::from_millis(50), q2.acquire("proj_a", 0)).await;
        assert!(
            blocked.is_err(),
            "proj_a second acquire should block (limit=1)"
        );

        // proj_b has no configured limit (uses global=4); can still acquire.
        let p_b = q.acquire("proj_b", 0).await.unwrap();
        assert_eq!(q.running_count(), 2);
        drop(p_b);
    }

    #[tokio::test]
    async fn project_cannot_starve_another() {
        use std::collections::HashMap;
        // global=2, proj_a=1 → proj_a can use at most 1 global slot,
        // leaving at least 1 for proj_b.
        let mut per_project = HashMap::new();
        per_project.insert("proj_a".to_string(), 1usize);
        let cfg = ConcurrencyConfig {
            max_concurrent_tasks: 2,
            max_queue_size: 16,
            stall_timeout_secs: 300,
            per_project,
            ..ConcurrencyConfig::default()
        };
        let q = Arc::new(TaskQueue::new(&cfg));

        // proj_a fills its project slot (limit=1).
        let _pa = q.acquire("proj_a", 0).await.unwrap();

        // proj_a cannot take another slot even though 1 global slot remains.
        let q2 = q.clone();
        let blocked = timeout(Duration::from_millis(50), q2.acquire("proj_a", 0)).await;
        assert!(
            blocked.is_err(),
            "proj_a blocked at project limit while global slot is free"
        );

        // proj_b can still acquire the remaining global slot.
        let pb = timeout(Duration::from_millis(100), q.acquire("proj_b", 0)).await;
        assert!(pb.is_ok(), "proj_b should get the remaining global slot");
    }

    #[tokio::test]
    async fn project_stats_reflect_running_and_queued() {
        let q = Arc::new(TaskQueue::new(&config(4, 16)));

        let _p1 = q.acquire("stats_proj", 0).await.unwrap();
        let stats = q.project_stats("stats_proj");
        assert_eq!(stats.running, 1);
        assert_eq!(stats.queued, 0);

        // Queue a waiter by capping project limit to 1.
        use std::collections::HashMap;
        let mut per_project = HashMap::new();
        per_project.insert("capped".to_string(), 1usize);
        let cfg2 = ConcurrencyConfig {
            max_concurrent_tasks: 4,
            max_queue_size: 16,
            stall_timeout_secs: 300,
            per_project,
            ..ConcurrencyConfig::default()
        };
        let q2 = Arc::new(TaskQueue::new(&cfg2));
        let _holder = q2.acquire("capped", 0).await.unwrap();
        let q3 = q2.clone();
        let _waiter = tokio::spawn(async move { q3.acquire("capped", 0).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let stats2 = q2.project_stats("capped");
        assert_eq!(stats2.running, 1);
        assert_eq!(stats2.queued, 1);
    }

    // --- New priority tests ---

    #[tokio::test]
    async fn high_priority_acquired_before_low_when_slots_full() {
        // 1 slot: hold it, then enqueue priority=0 then priority=1.
        // When the slot is released, priority=1 should unblock first.
        let q = Arc::new(TaskQueue::new(&config(1, 16)));
        let holder = q.acquire("proj", 0).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u8>();

        let q1 = q.clone();
        let tx1 = tx.clone();
        tokio::spawn(async move {
            let _p = q1.acquire("proj", 0).await.unwrap();
            let _ = tx1.send(0);
        });

        // Small delay to ensure priority=0 waiter is registered first.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let q2 = q.clone();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let _p = q2.acquire("proj", 1).await.unwrap();
            let _ = tx2.send(1);
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        // Release the holder; the priority=1 waiter should unblock first.
        drop(holder);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let first = rx.recv().await.expect("should receive first");
        assert_eq!(first, 1, "priority=1 task should unblock before priority=0");
    }

    #[tokio::test]
    async fn same_priority_is_fifo() {
        // 1 slot: hold it, then enqueue three priority=1 waiters in order.
        // They should unblock in enqueue order (FIFO).
        let q = Arc::new(TaskQueue::new(&config(1, 16)));
        let holder = q.acquire("proj", 1).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u8>();

        for i in 0u8..3 {
            let q_i = q.clone();
            let tx_i = tx.clone();
            tokio::spawn(async move {
                let _p = q_i.acquire("proj", 1).await.unwrap();
                let _ = tx_i.send(i);
                // Hold slot briefly so ordering is observable.
                tokio::time::sleep(Duration::from_millis(20)).await;
            });
            // Stagger enqueue so seq ordering is deterministic.
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Release the holder.
        drop(holder);
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut order = Vec::new();
        while let Ok(v) = rx.try_recv() {
            order.push(v);
        }
        assert_eq!(order, vec![0, 1, 2], "same-priority waiters must be FIFO");
    }

    #[tokio::test]
    async fn priority_zero_default_compiles() {
        let q = TaskQueue::new(&config(2, 8));
        let p = q.acquire("proj", 0).await;
        assert!(p.is_ok());
    }
}
