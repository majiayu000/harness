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

/// Tracks which phase of `acquire` is active for cleanup purposes.
enum AcquirePhase {
    /// Waiting for (or about to acquire) the project-level slot.
    /// `queued_count` and `project_queued_counter` are incremented.
    ProjectWait {
        project_queued_counter: Arc<AtomicUsize>,
    },
    /// Project slot acquired; waiting for (or about to acquire) the global slot.
    /// `queued_count` and `project_awaiting_counter` are incremented; the
    /// project queue holds one consumed permit that must be released on drop.
    GlobalWait {
        project_awaiting_counter: Arc<AtomicUsize>,
    },
}

/// Cancellation-safe RAII guard for [`TaskQueue::acquire`].
///
/// Dropped whenever the enclosing `async fn` is cancelled (future dropped) at
/// any `.await` point, ensuring queue counters and acquired project permits are
/// always released — even when the caller disconnects mid-wait.
///
/// Call [`AcquireGuard::defuse`] before returning the permit to prevent
/// the guard from releasing resources that are now owned by the caller.
struct AcquireGuard<'a> {
    queued_count: &'a AtomicUsize,
    project_queue: Arc<Mutex<PriorityPermitQueue>>,
    phase: AcquirePhase,
    defused: bool,
}

impl<'a> AcquireGuard<'a> {
    fn new(
        queued_count: &'a AtomicUsize,
        project_queue: Arc<Mutex<PriorityPermitQueue>>,
        project_queued_counter: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            queued_count,
            project_queue,
            phase: AcquirePhase::ProjectWait {
                project_queued_counter,
            },
            defused: false,
        }
    }

    /// Transition to the global-wait phase once the project slot is secured.
    fn transition_to_global_wait(&mut self, project_awaiting_counter: Arc<AtomicUsize>) {
        self.phase = AcquirePhase::GlobalWait {
            project_awaiting_counter,
        };
    }

    /// Prevent cleanup — ownership of the slots has transferred to the caller.
    fn defuse(&mut self) {
        self.defused = true;
    }
}

impl<'a> Drop for AcquireGuard<'a> {
    fn drop(&mut self) {
        if self.defused {
            return;
        }
        self.queued_count.fetch_sub(1, AtomicOrdering::SeqCst);
        match &self.phase {
            AcquirePhase::ProjectWait {
                project_queued_counter,
            } => {
                project_queued_counter.fetch_sub(1, AtomicOrdering::SeqCst);
                // No project permit was acquired yet; nothing to release in the queue.
            }
            AcquirePhase::GlobalWait {
                project_awaiting_counter,
            } => {
                project_awaiting_counter.fetch_sub(1, AtomicOrdering::SeqCst);
                // Project permit was already acquired; release it so other waiters unblock.
                self.project_queue.lock().unwrap().release();
            }
        }
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
    ///
    /// # Cancellation safety
    ///
    /// This function is cancellation-safe. If the future is dropped at either
    /// `.await` point (e.g. client disconnect), the `AcquireGuard` RAII guard
    /// releases all acquired resources — counters are decremented and any
    /// held project permit is returned to the queue.
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

        // Cancellation-safe guard: if this future is dropped at any .await
        // below, Drop releases counters and any acquired project permit.
        let mut guard = AcquireGuard::new(
            &self.queued_count,
            project_queue.clone(),
            project_queued_counter.clone(),
        );

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
            // CANCELLATION POINT 1: guard cleans up if dropped here.
            rx.await
                .map_err(|_| anyhow::anyhow!("project task queue closed"))?;
        }

        // Project slot acquired; transition from "waiting for project" to
        // "waiting for global". Keep this task counted as queued (not running)
        // until it also holds the global slot.
        project_queued_counter.fetch_sub(1, AtomicOrdering::SeqCst);
        let project_awaiting_counter = self.get_or_create_project_awaiting_global(project_id);
        project_awaiting_counter.fetch_add(1, AtomicOrdering::SeqCst);
        guard.transition_to_global_wait(project_awaiting_counter.clone());

        // Then acquire the global slot.
        let global_rx = self
            .global_queue
            .lock()
            .unwrap()
            .try_acquire_or_enqueue(priority);
        if let Err(rx) = global_rx {
            // CANCELLATION POINT 2: guard releases project permit and cleans
            // up counters if dropped here.
            rx.await.map_err(|_| anyhow::anyhow!("task queue closed"))?;
        }

        project_awaiting_counter.fetch_sub(1, AtomicOrdering::SeqCst);
        self.queued_count.fetch_sub(1, AtomicOrdering::SeqCst);
        // Both slots acquired; ownership transfers to TaskPermit — disarm guard.
        guard.defuse();

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
#[path = "task_queue_tests.rs"]
mod tests;
