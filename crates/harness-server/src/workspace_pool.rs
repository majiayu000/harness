use crate::workspace::workspace_helpers::{fnv1a_8, sanitize_repo_slug};
use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

const DEFAULT_WORKSPACE_POOL_CAPACITY: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspacePoolConfig {
    default_capacity: usize,
    per_project: HashMap<String, usize>,
}

impl Default for WorkspacePoolConfig {
    fn default() -> Self {
        Self {
            default_capacity: DEFAULT_WORKSPACE_POOL_CAPACITY,
            per_project: HashMap::new(),
        }
    }
}

impl WorkspacePoolConfig {
    pub(crate) fn new(default_capacity: usize, per_project: HashMap<String, usize>) -> Self {
        Self {
            default_capacity: default_capacity.max(1),
            per_project: per_project
                .into_iter()
                .map(|(key, value)| (key, value.max(1)))
                .collect(),
        }
    }

    fn capacity_for(&self, source_repo: &Path) -> usize {
        let project_key = project_limit_key(source_repo);
        self.per_project
            .get(&project_key)
            .copied()
            .unwrap_or(self.default_capacity)
            .max(1)
    }
}

pub(crate) struct WorkspacePool {
    config: WorkspacePoolConfig,
    semaphores: DashMap<String, Arc<Semaphore>>,
    slot_locks: DashMap<String, Arc<Mutex<()>>>,
}

impl WorkspacePool {
    pub(crate) fn new(config: WorkspacePoolConfig) -> Self {
        Self {
            config,
            semaphores: DashMap::new(),
            slot_locks: DashMap::new(),
        }
    }

    pub(crate) async fn acquire(
        &self,
        source_repo: &Path,
        repo: Option<&str>,
    ) -> anyhow::Result<WorkspacePoolPermit> {
        let capacity = self.config.capacity_for(source_repo);
        let project_key = derive_workspace_pool_key(source_repo, repo);
        let semaphore = self
            .semaphores
            .entry(project_key.clone())
            .or_insert_with(|| Arc::new(Semaphore::new(capacity)))
            .clone();
        let permit = semaphore.acquire_owned().await.map_err(|_| {
            anyhow::anyhow!("workspace pool semaphore closed for project {project_key}")
        })?;
        Ok(WorkspacePoolPermit {
            project_key,
            capacity,
            permit,
        })
    }

    pub(crate) fn selection_lock(&self, project_key: &str) -> Arc<Mutex<()>> {
        self.slot_locks
            .entry(project_key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

pub(crate) struct WorkspacePoolPermit {
    pub(crate) project_key: String,
    pub(crate) capacity: usize,
    pub(crate) permit: OwnedSemaphorePermit,
}

pub(crate) fn select_available_slot(capacity: usize, occupied: &HashSet<u32>) -> Option<u32> {
    (0..capacity as u32).find(|slot| !occupied.contains(slot))
}

pub(crate) fn workspace_slot_key(project_key: &str, slot_index: u32) -> String {
    format!("{project_key}__slot_{slot_index:03}")
}

pub(crate) fn project_limit_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn derive_workspace_pool_key(source_repo: &Path, repo: Option<&str>) -> String {
    let canonical = source_repo
        .canonicalize()
        .unwrap_or_else(|_| source_repo.to_path_buf());
    let project_hash = fnv1a_8(&canonical.to_string_lossy());
    let repo_key = repo
        .filter(|repo| !repo.is_empty())
        .map(sanitize_repo_slug)
        .unwrap_or_else(|| "project".to_string());
    format!("{project_hash}__{repo_key}__pool")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_available_slot_uses_first_free_slot() {
        let occupied = HashSet::from([0, 2]);
        assert_eq!(select_available_slot(4, &occupied), Some(1));
    }

    #[test]
    fn select_available_slot_returns_none_when_full() {
        let occupied = HashSet::from([0, 1]);
        assert_eq!(select_available_slot(2, &occupied), None);
    }
}
