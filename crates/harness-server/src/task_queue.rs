use dashmap::DashMap;
use harness_core::config::misc::ConcurrencyConfig;
use serde::Serialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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
    _global_permit: OwnedSemaphorePermit,
    _project_permit: OwnedSemaphorePermit,
}

impl std::fmt::Debug for TaskPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskPermit").finish()
    }
}

/// Bounded task queue using semaphores for concurrency control.
///
/// Maintains a global semaphore limiting total concurrent tasks across all projects,
/// plus per-project semaphores so one project cannot starve others. Acquiring a slot
/// requires holding BOTH permits simultaneously.
pub struct TaskQueue {
    global_semaphore: Arc<Semaphore>,
    global_limit: usize,
    max_queue_size: usize,
    queued_count: AtomicUsize,
    /// Per-project execution semaphores. Created on first use.
    project_semaphores: DashMap<String, Arc<Semaphore>>,
    /// Configured per-project max concurrent limits. Missing entries fall back to `global_limit`.
    project_limits: DashMap<String, usize>,
    /// Per-project count of tasks waiting for a project-level slot.
    project_queued: DashMap<String, Arc<AtomicUsize>>,
    /// Per-project count of tasks that hold the project slot but are still
    /// waiting for the global slot. These are "queued" not "running" from an
    /// observability standpoint — they haven't started executing yet.
    project_awaiting_global: DashMap<String, Arc<AtomicUsize>>,
}

impl TaskQueue {
    pub fn new(config: &ConcurrencyConfig) -> Self {
        let project_limits: DashMap<String, usize> = config
            .per_project
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        Self {
            global_semaphore: Arc::new(Semaphore::new(config.max_concurrent_tasks)),
            global_limit: config.max_concurrent_tasks,
            max_queue_size: config.max_queue_size,
            queued_count: AtomicUsize::new(0),
            project_semaphores: DashMap::new(),
            project_limits,
            project_queued: DashMap::new(),
            project_awaiting_global: DashMap::new(),
        }
    }

    fn project_limit(&self, project_id: &str) -> usize {
        self.project_limits
            .get(project_id)
            .map(|v| *v)
            .unwrap_or(self.global_limit)
    }

    fn get_or_create_project_semaphore(&self, project_id: &str) -> Arc<Semaphore> {
        self.project_semaphores
            .entry(project_id.to_string())
            .or_insert_with(|| {
                let limit = self.project_limit(project_id);
                Arc::new(Semaphore::new(limit))
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

    /// Acquire a global + project-level execution slot.
    ///
    /// Acquiring a project permit first prevents a single project from
    /// monopolizing all global slots while waiting for its own limit.
    ///
    /// Returns `Err` immediately if `max_queue_size` tasks are already waiting.
    /// The returned permit releases both slots on drop (even on panic).
    pub async fn acquire(&self, project_id: &str) -> anyhow::Result<TaskPermit> {
        // Reserve a global queue slot. fetch_add returns value before increment,
        // so if prev == max_queue_size the queue is already full.
        let prev = self.queued_count.fetch_add(1, Ordering::SeqCst);
        if prev >= self.max_queue_size {
            self.queued_count.fetch_sub(1, Ordering::SeqCst);
            return Err(anyhow::anyhow!(
                "task queue is full (max_queue_size={})",
                self.max_queue_size
            ));
        }

        let project_sem = self.get_or_create_project_semaphore(project_id);
        let project_queued_counter = self.get_or_create_project_queued(project_id);
        project_queued_counter.fetch_add(1, Ordering::SeqCst);

        // Acquire the project-level slot first. This ensures a project cannot
        // hold global slots while waiting for its own concurrency limit,
        // preventing one project from starving others.
        let project_permit = match project_sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                self.queued_count.fetch_sub(1, Ordering::SeqCst);
                project_queued_counter.fetch_sub(1, Ordering::SeqCst);
                return Err(anyhow::anyhow!("project task queue closed"));
            }
        };
        // Project slot acquired; transition from "waiting for project" to
        // "waiting for global". Keep this task counted as queued (not running)
        // until it also holds the global slot.
        project_queued_counter.fetch_sub(1, Ordering::SeqCst);
        let project_awaiting_counter = self.get_or_create_project_awaiting_global(project_id);
        project_awaiting_counter.fetch_add(1, Ordering::SeqCst);

        // Then acquire the global slot.
        let global_permit = match self.global_semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                self.queued_count.fetch_sub(1, Ordering::SeqCst);
                project_awaiting_counter.fetch_sub(1, Ordering::SeqCst);
                return Err(anyhow::anyhow!("task queue closed"));
            }
        };
        project_awaiting_counter.fetch_sub(1, Ordering::SeqCst);
        self.queued_count.fetch_sub(1, Ordering::SeqCst);

        Ok(TaskPermit {
            _global_permit: global_permit,
            _project_permit: project_permit,
        })
    }

    /// Number of tasks currently holding a global execution slot (running).
    pub fn running_count(&self) -> usize {
        self.global_limit
            .saturating_sub(self.global_semaphore.available_permits())
    }

    /// Number of tasks waiting for a global execution slot.
    pub fn queued_count(&self) -> usize {
        self.queued_count.load(Ordering::Relaxed)
    }

    /// Global execution limit.
    pub fn global_limit(&self) -> usize {
        self.global_limit
    }

    /// Stats for a specific project (tasks that have acquired a project slot).
    pub fn project_stats(&self, project_id: &str) -> QueueStats {
        let limit = self.project_limit(project_id);
        if let Some(sem) = self.project_semaphores.get(project_id) {
            // Tasks that hold the project semaphore but haven't yet acquired
            // the global semaphore are still waiting — count them as queued,
            // not running. "running" means the task holds BOTH permits and is
            // actively executing.
            let awaiting_global = self
                .project_awaiting_global
                .get(project_id)
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(0);
            let holding_project = limit.saturating_sub(sem.available_permits());
            let running = holding_project.saturating_sub(awaiting_global);
            let queued = self
                .project_queued
                .get(project_id)
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(0)
                + awaiting_global;
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
        self.project_semaphores
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
        let permit = q.acquire("proj").await.unwrap();
        assert_eq!(q.running_count(), 1);
        drop(permit);
        assert_eq!(q.running_count(), 0);
    }

    #[tokio::test]
    async fn only_max_concurrent_tasks_run_simultaneously() {
        let q = Arc::new(TaskQueue::new(&config(2, 16)));

        // Acquire both permits.
        let p1 = q.acquire("proj").await.unwrap();
        let p2 = q.acquire("proj").await.unwrap();
        assert_eq!(q.running_count(), 2);

        // Third acquire must block; verify with a short timeout.
        let q2 = q.clone();
        let blocked = timeout(Duration::from_millis(50), q2.acquire("proj")).await;
        assert!(blocked.is_err(), "third acquire should block when limit=2");

        // Release a slot; third acquire should now succeed.
        drop(p1);
        let p3 = q.acquire("proj").await.unwrap();
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
        let _p1 = q.acquire("proj").await.unwrap();

        // Two tasks queue up (they block, so spawn them).
        let q2 = q.clone();
        let _h1 = tokio::spawn(async move { q2.acquire("proj").await });
        let q3 = q.clone();
        let _h2 = tokio::spawn(async move { q3.acquire("proj").await });

        // Give spawned tasks time to increment queued_count.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Third waiter exceeds max_queue_size=2 → error.
        let result = q.acquire("proj").await;
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
                let _permit = q_inner.acquire("proj").await.unwrap();
                panic!("forced panic to test permit drop");
            });
            let _ = handle.await; // ignore the JoinError from the panic
        }

        // The permit must have been released; acquiring again should succeed immediately.
        let result = timeout(Duration::from_millis(100), q.acquire("proj")).await;
        assert!(result.is_ok(), "permit should be released after panic");
    }

    #[tokio::test]
    async fn queued_count_increments_while_waiting() {
        let q = Arc::new(TaskQueue::new(&config(1, 8)));

        // Hold the slot so the next acquire must wait.
        let _holder = q.acquire("proj").await.unwrap();
        assert_eq!(q.queued_count(), 0);

        let q2 = q.clone();
        let _waiter = tokio::spawn(async move { q2.acquire("proj").await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(q.queued_count(), 1);
    }

    #[tokio::test]
    async fn running_count_and_queued_count_reset_after_completion() {
        let q = TaskQueue::new(&config(4, 16));
        let p1 = q.acquire("proj").await.unwrap();
        let p2 = q.acquire("proj").await.unwrap();
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
        let _p1 = q.acquire("proj_a").await.unwrap();
        let q2 = q.clone();
        let blocked = timeout(Duration::from_millis(50), q2.acquire("proj_a")).await;
        assert!(
            blocked.is_err(),
            "proj_a second acquire should block (limit=1)"
        );

        // proj_b has no configured limit (uses global=4); can still acquire.
        let p_b = q.acquire("proj_b").await.unwrap();
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
        let _pa = q.acquire("proj_a").await.unwrap();

        // proj_a cannot take another slot even though 1 global slot remains.
        let q2 = q.clone();
        let blocked = timeout(Duration::from_millis(50), q2.acquire("proj_a")).await;
        assert!(
            blocked.is_err(),
            "proj_a blocked at project limit while global slot is free"
        );

        // proj_b can still acquire the remaining global slot.
        let pb = timeout(Duration::from_millis(100), q.acquire("proj_b")).await;
        assert!(pb.is_ok(), "proj_b should get the remaining global slot");
    }

    #[tokio::test]
    async fn project_stats_reflect_running_and_queued() {
        let q = Arc::new(TaskQueue::new(&config(4, 16)));

        let _p1 = q.acquire("stats_proj").await.unwrap();
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
        let _holder = q2.acquire("capped").await.unwrap();
        let q3 = q2.clone();
        let _waiter = tokio::spawn(async move { q3.acquire("capped").await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let stats2 = q2.project_stats("capped");
        assert_eq!(stats2.running, 1);
        assert_eq!(stats2.queued, 1);
    }
}
