use crate::runtime_project_cache::{
    CachedProjectRecord, HostProjectCacheRecord, RuntimeProjectCacheManager,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedWatchedProject {
    pub project_id: Option<String>,
    pub root: String,
    pub synced_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedHostProjectCache {
    pub host_id: String,
    pub last_synced_at: DateTime<Utc>,
    pub projects: Vec<PersistedWatchedProject>,
}

impl RuntimeProjectCacheManager {
    pub fn snapshot_state(&self) -> Vec<PersistedHostProjectCache> {
        let mut snapshots: Vec<PersistedHostProjectCache> = self
            .caches
            .iter()
            .map(|entry| {
                let cache = entry.value();
                PersistedHostProjectCache {
                    host_id: entry.key().clone(),
                    last_synced_at: cache.last_synced_at,
                    projects: cache
                        .projects
                        .iter()
                        .map(|project| PersistedWatchedProject {
                            project_id: project.project_id.clone(),
                            root: project.root.clone(),
                            synced_at: project.synced_at,
                        })
                        .collect(),
                }
            })
            .collect();
        snapshots.sort_by(|a, b| a.host_id.cmp(&b.host_id));
        snapshots
    }

    pub fn restore_state(&self, caches: Vec<PersistedHostProjectCache>) -> usize {
        self.caches.clear();

        for cache in caches {
            if cache.host_id.trim().is_empty() {
                continue;
            }
            let mut dedup: BTreeMap<String, CachedProjectRecord> = BTreeMap::new();
            for project in cache.projects {
                dedup.insert(
                    project.root.clone(),
                    CachedProjectRecord {
                        project_id: project.project_id,
                        root: project.root,
                        synced_at: project.synced_at,
                    },
                );
            }
            self.caches.insert(
                cache.host_id,
                HostProjectCacheRecord {
                    last_synced_at: cache.last_synced_at,
                    projects: dedup.into_values().collect(),
                },
            );
        }

        self.caches.len()
    }
}
