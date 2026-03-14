use dashmap::DashMap;
use harness_core::config::ConcurrencyConfig;
use serde::Serialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone, Serialize)]
pub struct QueueStats {
    pub running: usize,
    pub queued: usize,
    pub limit: usize,
}

pub struct TaskPermit {
    _global_permit: OwnedSemaphorePermit,
    _project_permit: OwnedSemaphorePermit,
}

impl std::fmt::Debug for TaskPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskPermit").finish()
    }
}

pub struct TaskQueue {
    global_semaphore: Arc<Semaphore>,
    global_limit: usize,
    max_queue_size: usize,
    queued_count: AtomicUsize,
    project_semaphores: DashMap<String, Arc<Semaphore>>,
    project_limits: DashMap<String, usize>,
    project_queued: DashMap<String, Arc<AtomicUsize>>,
    project_awaiting_global: DashMap<String, Arc<AtomicUsize>>,
    /// Per-process random state used to hash canonical project paths into opaque
    /// identifiers for the monitoring API, preventing filesystem path enumeration.
    hasher_state: std::collections::hash_map::RandomState,
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
            hasher_state: std::collections::hash_map::RandomState::new(),
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

    pub async fn acquire(&self, project_id: &str) -> anyhow::Result<TaskPermit> {
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

        let project_permit = match project_sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                self.queued_count.fetch_sub(1, Ordering::SeqCst);
                project_queued_counter.fetch_sub(1, Ordering::SeqCst);
                return Err(anyhow::anyhow!("project task queue closed"));
            }
        };
        project_queued_counter.fetch_sub(1, Ordering::SeqCst);
        let project_awaiting_counter = self.get_or_create_project_awaiting_global(project_id);
        project_awaiting_counter.fetch_add(1, Ordering::SeqCst);

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

    pub fn running_count(&self) -> usize {
        self.global_limit
            .saturating_sub(self.global_semaphore.available_permits())
    }

    pub fn queued_count(&self) -> usize {
        self.queued_count.load(Ordering::Relaxed)
    }

    pub fn global_limit(&self) -> usize {
        self.global_limit
    }

    pub fn project_stats(&self, project_id: &str) -> QueueStats {
        let limit = self.project_limit(project_id);
        if let Some(sem) = self.project_semaphores.get(project_id) {
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

    /// Produce a stable per-process opaque key for a project path.
    /// Uses a random seed so the same path always maps to the same key
    /// within one process lifetime but is unpredictable across restarts,
    /// preventing offline path enumeration via the monitoring endpoint.
    fn opaque_key(&self, path: &str) -> String {
        use std::hash::{BuildHasher, Hash, Hasher};
        let mut h = self.hasher_state.build_hasher();
        path.hash(&mut h);
        format!("{:016x}", h.finish())
    }

    /// Stats for all projects that have been used at least once.
    /// Project keys are opaque identifiers (per-process hash of the path) to
    /// avoid exposing filesystem paths to unauthenticated monitoring clients.
    pub fn all_project_stats(&self) -> Vec<(String, QueueStats)> {
        self.project_semaphores
            .iter()
            .map(|entry| {
                let project_id = entry.key().clone();
                let stats = self.project_stats(&project_id);
                let opaque = self.opaque_key(&project_id);
                (opaque, stats)
            })
            .collect()
    }

    /// Remove per-project queue entries that are completely idle (no running or
    /// queued tasks). Call this after a task fails validation to reclaim entries
    /// that should never have been created.
    pub fn prune_idle_projects(&self) {
        let idle: Vec<String> = self
            .project_semaphores
            .iter()
            .filter_map(|entry| {
                let id = entry.key();
                let sem = entry.value();
                let limit = self.project_limit(id);
                if sem.available_permits() < limit {
                    return None; // tasks still running
                }
                let queued = self
                    .project_queued
                    .get(id)
                    .map(|c| c.load(Ordering::Relaxed))
                    .unwrap_or(0);
                let awaiting = self
                    .project_awaiting_global
                    .get(id)
                    .map(|c| c.load(Ordering::Relaxed))
                    .unwrap_or(0);
                if queued == 0 && awaiting == 0 {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();
        for id in idle {
            self.project_semaphores.remove(&id);
            self.project_queued.remove(&id);
            self.project_awaiting_global.remove(&id);
        }
    }

    #[cfg(test)]
    pub fn unbounded() -> Self {
        Self::new(&ConcurrencyConfig {
            max_concurrent_tasks: 1024,
            max_queue_size: 1024,
            stall_timeout_secs: 300,
            per_project: Default::default(),
        })
    }
}
