use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct WatchedProjectInput {
    pub project_id: Option<String>,
    pub root: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WatchedProject {
    pub project_id: Option<String>,
    pub root: String,
    pub synced_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostProjectCacheSnapshot {
    pub host_id: String,
    pub last_synced_at: String,
    pub project_count: usize,
    pub projects: Vec<WatchedProject>,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedProjectRecord {
    pub(crate) project_id: Option<String>,
    pub(crate) root: String,
    pub(crate) synced_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct HostProjectCacheRecord {
    pub(crate) last_synced_at: DateTime<Utc>,
    pub(crate) projects: Vec<CachedProjectRecord>,
}

pub struct RuntimeProjectCacheManager {
    pub(crate) caches: DashMap<String, HostProjectCacheRecord>,
}

impl RuntimeProjectCacheManager {
    pub fn new() -> Self {
        Self {
            caches: DashMap::new(),
        }
    }

    pub fn sync_host_projects(
        &self,
        host_id: &str,
        projects: Vec<WatchedProjectInput>,
    ) -> HostProjectCacheSnapshot {
        let now = Utc::now();
        let mut dedup: BTreeMap<String, CachedProjectRecord> = BTreeMap::new();
        for project in projects {
            dedup.insert(
                project.root.clone(),
                CachedProjectRecord {
                    project_id: project.project_id,
                    root: project.root,
                    synced_at: now,
                },
            );
        }
        let cache = HostProjectCacheRecord {
            last_synced_at: now,
            projects: dedup.into_values().collect(),
        };
        self.caches.insert(host_id.to_string(), cache.clone());
        self.to_snapshot(host_id, &cache)
    }

    pub fn get_host_cache(&self, host_id: &str) -> Option<HostProjectCacheSnapshot> {
        self.caches
            .get(host_id)
            .map(|cache| self.to_snapshot(host_id, &cache))
    }

    pub fn empty_snapshot(&self, host_id: &str) -> HostProjectCacheSnapshot {
        HostProjectCacheSnapshot {
            host_id: host_id.to_string(),
            last_synced_at: Utc::now().to_rfc3339(),
            project_count: 0,
            projects: vec![],
        }
    }

    pub fn clear_host(&self, host_id: &str) -> bool {
        self.caches.remove(host_id).is_some()
    }

    fn to_snapshot(
        &self,
        host_id: &str,
        cache: &HostProjectCacheRecord,
    ) -> HostProjectCacheSnapshot {
        HostProjectCacheSnapshot {
            host_id: host_id.to_string(),
            last_synced_at: cache.last_synced_at.to_rfc3339(),
            project_count: cache.projects.len(),
            projects: cache
                .projects
                .iter()
                .map(|project| WatchedProject {
                    project_id: project.project_id.clone(),
                    root: project.root.clone(),
                    synced_at: project.synced_at.to_rfc3339(),
                })
                .collect(),
        }
    }
}
