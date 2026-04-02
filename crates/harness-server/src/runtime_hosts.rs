use crate::task_runner::{TaskId, TaskStatus};
use chrono::{DateTime, Duration, Utc};
use dashmap::{mapref::entry::Entry, DashMap};
use serde::Serialize;
use std::{
    cmp::{Ordering, Reverse},
    collections::{BinaryHeap, HashSet},
    sync::Mutex,
};

pub const DEFAULT_HEARTBEAT_TIMEOUT_SECS: i64 = 60;
pub const DEFAULT_LEASE_SECS: i64 = 60;

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeHostInfo {
    pub id: String,
    pub display_name: String,
    pub capabilities: Vec<String>,
    pub registered_at: String,
    pub last_heartbeat_at: String,
    pub online: bool,
}

#[derive(Debug, Clone)]
pub struct ClaimCandidate {
    pub task_id: TaskId,
    pub status: TaskStatus,
    pub created_at: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskClaimResult {
    pub task_id: TaskId,
    pub lease_expires_at: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeHostRecord {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) capabilities: Vec<String>,
    pub(crate) registered_at: DateTime<Utc>,
    pub(crate) last_heartbeat_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskLease {
    pub(crate) host_id: String,
    pub(crate) expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct LeaseExpiry {
    pub(crate) expires_at: DateTime<Utc>,
    pub(crate) task_id: TaskId,
}

impl Ord for LeaseExpiry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.expires_at
            .cmp(&other.expires_at)
            .then_with(|| self.task_id.0.cmp(&other.task_id.0))
    }
}

impl PartialOrd for LeaseExpiry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub struct RuntimeHostManager {
    pub(crate) hosts: DashMap<String, RuntimeHostRecord>,
    pub(crate) leases: DashMap<TaskId, TaskLease>,
    pub(crate) host_leases: DashMap<String, HashSet<TaskId>>,
    pub(crate) lease_expirations: Mutex<BinaryHeap<Reverse<LeaseExpiry>>>,
    pub(crate) heartbeat_timeout_secs: i64,
    default_lease_secs: i64,
}

impl RuntimeHostManager {
    pub fn new() -> Self {
        Self::with_timeouts(DEFAULT_HEARTBEAT_TIMEOUT_SECS, DEFAULT_LEASE_SECS)
    }

    pub fn with_timeouts(heartbeat_timeout_secs: i64, default_lease_secs: i64) -> Self {
        Self {
            hosts: DashMap::new(),
            leases: DashMap::new(),
            host_leases: DashMap::new(),
            lease_expirations: Mutex::new(BinaryHeap::new()),
            heartbeat_timeout_secs,
            default_lease_secs,
        }
    }

    pub fn register(
        &self,
        host_id: String,
        display_name: Option<String>,
        capabilities: Vec<String>,
    ) -> RuntimeHostInfo {
        let now = Utc::now();
        let record = RuntimeHostRecord {
            id: host_id.clone(),
            display_name: display_name.unwrap_or_else(|| host_id.clone()),
            capabilities,
            registered_at: now,
            last_heartbeat_at: now,
        };
        self.hosts.insert(host_id, record.clone());
        self.to_info(&record, now)
    }

    pub fn heartbeat(&self, host_id: &str) -> anyhow::Result<RuntimeHostInfo> {
        let now = Utc::now();
        let mut host = self
            .hosts
            .get_mut(host_id)
            .ok_or_else(|| anyhow::anyhow!("runtime host '{host_id}' is not registered"))?;
        host.last_heartbeat_at = now;
        Ok(self.to_info(&host, now))
    }

    pub fn deregister(&self, host_id: &str) -> bool {
        let removed = self.hosts.remove(host_id).is_some();
        if removed {
            let lease_ids = self
                .host_leases
                .remove(host_id)
                .map(|(_, ids)| ids)
                .unwrap_or_default();
            for task_id in lease_ids {
                self.leases.remove(&task_id);
            }
        }
        removed
    }

    pub fn list_hosts(&self) -> Vec<RuntimeHostInfo> {
        let now = Utc::now();
        let mut hosts: Vec<RuntimeHostInfo> = self
            .hosts
            .iter()
            .map(|entry| self.to_info(entry.value(), now))
            .collect();
        hosts.sort_by(|a, b| a.id.cmp(&b.id));
        hosts
    }

    pub fn claim_task(
        &self,
        host_id: &str,
        mut candidates: Vec<ClaimCandidate>,
        lease_secs: Option<i64>,
        project_filter: Option<&str>,
    ) -> anyhow::Result<Option<TaskClaimResult>> {
        // Hold a read reference during claim so concurrent deregister() cannot
        // remove the host between membership check and lease insertion.
        let host_guard = self
            .hosts
            .get(host_id)
            .ok_or_else(|| anyhow::anyhow!("runtime host '{host_id}' is not registered"))?;

        let now = Utc::now();
        self.cleanup_expired_leases(now);
        candidates.retain(|c| c.status.as_ref() == "pending" && project_matches(c, project_filter));
        candidates.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.task_id.as_str().cmp(b.task_id.as_str()))
        });

        let ttl = lease_secs.unwrap_or(self.default_lease_secs).max(0);
        for candidate in candidates {
            match self.leases.entry(candidate.task_id.clone()) {
                Entry::Vacant(v) => {
                    let expires_at = now + Duration::seconds(ttl);
                    v.insert(TaskLease {
                        host_id: host_id.to_string(),
                        expires_at,
                    });
                    self.index_lease(host_id, candidate.task_id.clone(), expires_at);
                    drop(host_guard);
                    return Ok(Some(TaskClaimResult {
                        task_id: candidate.task_id,
                        lease_expires_at: expires_at.to_rfc3339(),
                    }));
                }
                Entry::Occupied(_) => continue,
            }
        }
        drop(host_guard);
        Ok(None)
    }

    fn cleanup_expired_leases(&self, now: DateTime<Utc>) {
        loop {
            let next = {
                let mut heap = self
                    .lease_expirations
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                match heap.peek() {
                    Some(Reverse(expiry)) if expiry.expires_at <= now => {
                        heap.pop().map(|Reverse(item)| item)
                    }
                    _ => None,
                }
            };
            let Some(expiry) = next else {
                break;
            };
            let should_remove = self
                .leases
                .get(&expiry.task_id)
                .map(|lease| lease.expires_at <= now && lease.expires_at == expiry.expires_at)
                .unwrap_or(false);
            if should_remove {
                self.remove_lease(&expiry.task_id);
            }
        }
    }

    pub(crate) fn index_lease(&self, host_id: &str, task_id: TaskId, expires_at: DateTime<Utc>) {
        self.host_leases
            .entry(host_id.to_string())
            .or_default()
            .insert(task_id.clone());
        self.lease_expirations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(Reverse(LeaseExpiry {
                expires_at,
                task_id,
            }));
    }

    fn remove_lease(&self, task_id: &TaskId) {
        let Some((_, lease)) = self.leases.remove(task_id) else {
            return;
        };
        if let Some(mut owned_ids) = self.host_leases.get_mut(&lease.host_id) {
            owned_ids.remove(task_id);
            if owned_ids.is_empty() {
                drop(owned_ids);
                self.host_leases.remove(&lease.host_id);
            }
        }
    }

    fn to_info(&self, record: &RuntimeHostRecord, now: DateTime<Utc>) -> RuntimeHostInfo {
        let online = (now - record.last_heartbeat_at).num_seconds() <= self.heartbeat_timeout_secs;
        RuntimeHostInfo {
            id: record.id.clone(),
            display_name: record.display_name.clone(),
            capabilities: record.capabilities.clone(),
            registered_at: record.registered_at.to_rfc3339(),
            last_heartbeat_at: record.last_heartbeat_at.to_rfc3339(),
            online,
        }
    }
}

fn project_matches(candidate: &ClaimCandidate, project_filter: Option<&str>) -> bool {
    match project_filter {
        None => true,
        Some(filter) => candidate.project.as_deref() == Some(filter),
    }
}
