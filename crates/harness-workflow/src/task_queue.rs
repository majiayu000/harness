use dashmap::DashMap;
use harness_core::config::misc::ConcurrencyConfig;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use tokio::time::Instant;

#[path = "task_queue_permits.rs"]
mod permits;

use permits::{empty_wait_stats, AgingParams, PriorityPermitQueue};
pub use permits::{PriorityWaitStats, MAX_PRIORITY_LEVEL};

/// Per-project and global queue statistics.
#[derive(Debug, Clone, Serialize)]
pub struct QueueStats {
    /// Number of tasks currently holding a project execution slot.
    pub running: usize,
    /// Number of tasks waiting for a project execution slot.
    pub queued: usize,
    /// Maximum concurrent tasks for this project (or globally).
    pub limit: usize,
    /// Wait-time metrics per base priority level for the project-stage queue.
    pub wait_ms_by_priority: Vec<PriorityWaitStats>,
}

/// Runtime queue pressure snapshot for diagnostics.
#[derive(Debug, Clone)]
pub struct QueueDiagnostics {
    pub global_running: usize,
    pub global_queued: usize,
    pub global_limit: usize,
    pub project_running: usize,
    pub project_waiting_for_project: usize,
    pub project_awaiting_global: usize,
    pub project_limit: usize,
    /// Wait-time metrics per base priority level for the global-stage queue.
    pub global_wait_ms_by_priority: Vec<PriorityWaitStats>,
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
/// Uses `std::sync::Mutex` (not tokio) so the critical section (waiter scan)
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
/// any `.await` point, ensuring queue counters and acquired permits are always
/// released — even when the caller disconnects mid-wait.
///
/// The `project_permit_granted` / `global_permit_granted` flags (shared with
/// the corresponding `Waiter` via `Arc<AtomicBool>`) let Drop detect the race
/// where `release()` already sent a permit signal but the future was cancelled
/// before `rx.await` could complete.  A CAS reclaims the permit in that case.
///
/// The `project_waiter_cancelled` / `global_waiter_cancelled` flags let Drop
/// eagerly mark a queue-resident waiter as cancelled so that `release()` can
/// skip it without attempting a channel send guaranteed to fail.
///
/// Call [`AcquireGuard::defuse`] before returning the permit to prevent
/// the guard from releasing resources that are now owned by the caller.
struct AcquireGuard<'a> {
    queued_count: &'a AtomicUsize,
    project_queue: Arc<Mutex<PriorityPermitQueue>>,
    global_queue: Arc<Mutex<PriorityPermitQueue>>,
    phase: AcquirePhase,
    defused: bool,
    /// Shared flag with the project-queue Waiter; set by release() before send.
    project_permit_granted: Option<Arc<AtomicBool>>,
    /// Shared flag with the global-queue Waiter; set by release() before send.
    global_permit_granted: Option<Arc<AtomicBool>>,
    /// Shared flag with the project-queue Waiter; set by Drop to signal
    /// cancellation so `release()` can skip the queue entry eagerly.
    project_waiter_cancelled: Option<Arc<AtomicBool>>,
    /// Shared flag with the global-queue Waiter; set by Drop to signal
    /// cancellation so `release()` can skip the queue entry eagerly.
    global_waiter_cancelled: Option<Arc<AtomicBool>>,
}

impl<'a> AcquireGuard<'a> {
    fn new(
        queued_count: &'a AtomicUsize,
        project_queue: Arc<Mutex<PriorityPermitQueue>>,
        global_queue: Arc<Mutex<PriorityPermitQueue>>,
        project_queued_counter: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            queued_count,
            project_queue,
            global_queue,
            phase: AcquirePhase::ProjectWait {
                project_queued_counter,
            },
            defused: false,
            project_permit_granted: None,
            global_permit_granted: None,
            project_waiter_cancelled: None,
            global_waiter_cancelled: None,
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
                // Mark queue-resident waiter as cancelled so release() skips it
                // eagerly instead of attempting a send guaranteed to fail.
                // Also drain all stale entries to prevent unbounded queue growth
                // under repeated enqueue/cancel patterns.
                if let Some(c) = &self.project_waiter_cancelled {
                    c.store(true, AtomicOrdering::SeqCst);
                    self.project_queue.lock().unwrap().drain_cancelled();
                }
                // If release() already sent the project permit signal but the
                // future was cancelled before rx.await completed, reclaim it.
                if let Some(granted) = &self.project_permit_granted {
                    if granted
                        .compare_exchange(
                            true,
                            false,
                            AtomicOrdering::SeqCst,
                            AtomicOrdering::SeqCst,
                        )
                        .is_ok()
                    {
                        self.project_queue.lock().unwrap().release();
                    }
                }
            }
            AcquirePhase::GlobalWait {
                project_awaiting_counter,
            } => {
                project_awaiting_counter.fetch_sub(1, AtomicOrdering::SeqCst);
                // Project permit was already acquired; always release it.
                self.project_queue.lock().unwrap().release();
                // Mark queue-resident global waiter as cancelled so release()
                // skips it eagerly instead of attempting a send guaranteed to fail.
                // Also drain all stale entries to prevent unbounded queue growth.
                if let Some(c) = &self.global_waiter_cancelled {
                    c.store(true, AtomicOrdering::SeqCst);
                    self.global_queue.lock().unwrap().drain_cancelled();
                }
                // If release() already sent the global permit signal but the
                // future was cancelled before rx.await completed, reclaim it.
                if let Some(granted) = &self.global_permit_granted {
                    if granted
                        .compare_exchange(
                            true,
                            false,
                            AtomicOrdering::SeqCst,
                            AtomicOrdering::SeqCst,
                        )
                        .is_ok()
                    {
                        self.global_queue.lock().unwrap().release();
                    }
                }
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
/// Within the same priority level, tasks are served FIFO.  With aging enabled
/// (default), long-waiting low-priority tasks are gradually boosted so a
/// steady high-priority stream cannot starve them indefinitely.
pub struct TaskQueue {
    global_queue: Arc<Mutex<PriorityPermitQueue>>,
    global_limit: usize,
    max_queue_size: usize,
    queued_count: AtomicUsize,
    /// Aging parameters shared by the global queue and every project queue.
    aging: AgingParams,
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
            .map(|(k, v)| {
                let p = std::path::Path::new(k);
                if !p.is_absolute() {
                    tracing::warn!(
                        key = k.as_str(),
                        "concurrency.per_project key is not an absolute path; \
                         it will not match any project at runtime — \
                         keys must be canonical filesystem paths (use ProjectId::from_path)"
                    );
                    return (k.clone(), *v);
                }
                // Canonicalize to resolve symlinks / `..` / trailing components so
                // the key matches the canonical path stored by the registry/queue.
                let canonical = std::fs::canonicalize(p)
                    .unwrap_or_else(|_| p.to_path_buf())
                    .to_string_lossy()
                    .into_owned();
                if canonical != *k {
                    tracing::debug!(
                        original = k.as_str(),
                        canonical = canonical.as_str(),
                        "concurrency.per_project key canonicalized"
                    );
                }
                (canonical, *v)
            })
            .collect();
        let aging = AgingParams::from_config(&config.aging);
        Self {
            global_queue: Arc::new(Mutex::new(PriorityPermitQueue::new(
                config.max_concurrent_tasks,
                aging,
            ))),
            global_limit: config.max_concurrent_tasks,
            max_queue_size: config.max_queue_size,
            queued_count: AtomicUsize::new(0),
            aging,
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
                Arc::new(Mutex::new(PriorityPermitQueue::new(limit, self.aging)))
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
    /// # Priority aging
    ///
    /// With aging enabled, a waiter's effective priority rises one level per
    /// configured interval of wait, capped at `base + max_boost_levels` and
    /// never above [`MAX_PRIORITY_LEVEL`].  The project stage and the global
    /// stage age **independently**: the per-stage wait bound under a steady
    /// stream of maximum-priority arrivals is `max_boost_levels × interval`
    /// plus one FIFO turn at the capped level, and the documented end-to-end
    /// bound is the **sum of both stage bounds**.  A waiter that is cancelled
    /// and retried re-enqueues with a fresh wait clock (no carryover).
    ///
    /// # Cancellation safety
    ///
    /// This function is cancellation-safe. If the future is dropped at either
    /// `.await` point (e.g. client disconnect), the `AcquireGuard` RAII guard
    /// releases all acquired resources — counters are decremented and any
    /// held project permit is returned to the queue.
    pub async fn acquire(&self, project_id: &str, priority: u8) -> anyhow::Result<TaskPermit> {
        // Reject immediately when system memory is below the configured threshold.
        if self
            .memory_pressure
            .as_ref()
            .is_some_and(|f| f.load(AtomicOrdering::Relaxed))
        {
            return Err(anyhow::anyhow!(
                "task rejected: available system memory is below the configured threshold"
            ));
        }

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
        // below, Drop releases counters and any acquired permits.
        let mut guard = AcquireGuard::new(
            &self.queued_count,
            project_queue.clone(),
            self.global_queue.clone(),
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
        if let Err((rx, granted, cancelled, enqueued_at)) = project_rx {
            // Store flags so Drop can mark the waiter cancelled and detect
            // mid-send cancellation.
            guard.project_permit_granted = Some(granted);
            guard.project_waiter_cancelled = Some(cancelled);
            // CANCELLATION POINT 1: guard cleans up if dropped here.
            rx.await
                .map_err(|_| anyhow::anyhow!("project task queue closed"))?;
            // Permit consumed successfully; clear flags so Drop skips reclaim.
            guard.project_permit_granted = None;
            guard.project_waiter_cancelled = None;
            // Record the wait sample only after the grant is consumed, so
            // cancelled/reclaimed grants never reach the metrics (see
            // `record_granted_wait`).
            let waited = Instant::now().saturating_duration_since(enqueued_at);
            // vibeguard-disable-next-line RS-03 -- Mutex::lock().unwrap() is the repo-exempt fail-fast pattern
            let mut pq = project_queue.lock().unwrap();
            pq.record_granted_wait(priority, waited);
            drop(pq);
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
        if let Err((rx, granted, cancelled, enqueued_at)) = global_rx {
            // Store flags so Drop can mark the waiter cancelled and detect
            // mid-send cancellation.
            guard.global_permit_granted = Some(granted);
            guard.global_waiter_cancelled = Some(cancelled);
            // CANCELLATION POINT 2: guard releases project permit and cleans
            // up counters if dropped here.
            rx.await.map_err(|_| anyhow::anyhow!("task queue closed"))?;
            // Permit consumed successfully; clear flags so Drop skips reclaim.
            guard.global_permit_granted = None;
            guard.global_waiter_cancelled = None;
            // Record the wait sample only after the grant is consumed (see above).
            let waited = Instant::now().saturating_duration_since(enqueued_at);
            // vibeguard-disable-next-line RS-03 -- Mutex::lock().unwrap() is the repo-exempt fail-fast pattern
            let mut gq = self.global_queue.lock().unwrap();
            gq.record_granted_wait(priority, waited);
            drop(gq);
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
    ///
    /// `global_limit - available_permits` gives the number of issued permits.
    /// Global waiters have not received a permit yet and do not reduce
    /// `available_permits`, so subtracting `waiter_count()` would undercount
    /// running tasks when there are waiters in the queue.
    pub fn running_count(&self) -> usize {
        let q = self.global_queue.lock().unwrap();
        self.global_limit.saturating_sub(q.available_permits())
    }

    /// Number of tasks waiting for a global execution slot.
    pub fn queued_count(&self) -> usize {
        self.queued_count.load(AtomicOrdering::Relaxed)
    }

    /// Global execution limit.
    pub fn global_limit(&self) -> usize {
        self.global_limit
    }

    /// Effective project limit after applying per-project overrides.
    pub fn effective_project_limit(&self, project_id: &str) -> usize {
        self.project_limit(project_id)
    }

    /// Upsert the effective limit for a project and reconfigure any existing
    /// project queue so future acquisitions honor the new cap.
    pub fn set_project_limit(&self, project_id: &str, limit: usize) {
        let old_limit = self.project_limit(project_id);
        self.project_limits.insert(project_id.to_string(), limit);
        if let Some(project_queue) = self.project_queues.get(project_id) {
            project_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reconfigure_capacity(old_limit, limit);
        }
    }

    /// Remove an explicit project override so the queue falls back to the
    /// global limit for future acquisitions.
    pub fn reset_project_limit(&self, project_id: &str) {
        let old_limit = self.project_limit(project_id);
        self.project_limits.remove(project_id);
        let new_limit = self.project_limit(project_id);
        if let Some(project_queue) = self.project_queues.get(project_id) {
            project_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reconfigure_capacity(old_limit, new_limit);
        }
    }

    /// Snapshot queue pressure for diagnostics and admission-failure logging.
    pub fn diagnostics(&self, project_id: &str) -> QueueDiagnostics {
        let project_limit = self.project_limit(project_id);
        let global_running = self.running_count();
        let global_queued = self.queued_count();
        // vibeguard-disable-next-line RS-03 -- Mutex::lock().unwrap() is the repo-exempt fail-fast pattern
        let global_wait_ms_by_priority = self.global_queue.lock().unwrap().wait_stats_snapshot();
        let project_awaiting_global = self
            .project_awaiting_global
            .get(project_id)
            .map(|c| c.load(AtomicOrdering::Relaxed))
            .unwrap_or(0);
        if let Some(pq) = self.project_queues.get(project_id) {
            let q = pq.lock().unwrap_or_else(|e| e.into_inner());
            let holding_project = project_limit.saturating_sub(q.available_permits());
            let project_waiting_for_project = q.waiter_count();
            let project_running = holding_project.saturating_sub(project_awaiting_global);
            QueueDiagnostics {
                global_running,
                global_queued,
                global_limit: self.global_limit,
                project_running,
                project_waiting_for_project,
                project_awaiting_global,
                project_limit,
                global_wait_ms_by_priority,
            }
        } else {
            QueueDiagnostics {
                global_running,
                global_queued,
                global_limit: self.global_limit,
                project_running: 0,
                project_waiting_for_project: 0,
                project_awaiting_global,
                project_limit,
                global_wait_ms_by_priority,
            }
        }
    }

    /// Stats for a specific project (tasks that have acquired a project slot).
    pub fn project_stats(&self, project_id: &str) -> QueueStats {
        let limit = self.project_limit(project_id);
        if let Some(pq) = self.project_queues.get(project_id) {
            let q = pq.lock().unwrap();
            let holding_project = limit.saturating_sub(q.available_permits());
            let waiting_for_project = q.waiter_count();
            let wait_ms_by_priority = q.wait_stats_snapshot();
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
                wait_ms_by_priority,
            }
        } else {
            QueueStats {
                running: 0,
                queued: 0,
                limit,
                wait_ms_by_priority: empty_wait_stats(),
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

    /// Create a practically unbounded queue (high limits for testing or dev use).
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

#[cfg(test)]
#[path = "task_queue_aging_tests.rs"]
mod aging_tests;
