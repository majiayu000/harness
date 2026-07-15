use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Weak};
use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;

pub const DEFAULT_HEARTBEAT_TIMEOUT_SECS: i64 = 60;
pub const DEFAULT_LEASE_SECS: i64 = 60;
pub const MAX_LEASE_SECS: i64 = 3600;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeHostLifecycle {
    #[default]
    Active,
    Draining,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeHostInfo {
    pub id: String,
    pub display_name: String,
    pub capabilities: Vec<String>,
    pub registered_at: String,
    pub last_heartbeat_at: String,
    pub online: bool,
    pub lifecycle: RuntimeHostLifecycle,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskClaimResult {
    pub task_id: crate::task_runner::TaskId,
    pub lease_expires_at: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeHostRecord {
    pub(crate) id: String,
    pub(crate) registration_id: Uuid,
    pub(crate) display_name: String,
    pub(crate) capabilities: Vec<String>,
    pub(crate) registered_at: DateTime<Utc>,
    pub(crate) last_heartbeat_at: DateTime<Utc>,
    pub(crate) lifecycle: RuntimeHostLifecycle,
}

pub struct RuntimeHostManager {
    pub(crate) hosts: DashMap<String, RuntimeHostRecord>,
    operation_locks: Arc<DashMap<String, Weak<Mutex<()>>>>,
    pub(crate) heartbeat_timeout_secs: i64,
}

pub struct RuntimeHostOperationGuard {
    guard: Option<OwnedMutexGuard<()>>,
    operation_locks: Arc<DashMap<String, Weak<Mutex<()>>>>,
    host_id: String,
    operation_lock: Weak<Mutex<()>>,
}

impl Drop for RuntimeHostOperationGuard {
    fn drop(&mut self) {
        drop(self.guard.take());
        if self.operation_lock.strong_count() != 0 {
            return;
        }
        if let dashmap::mapref::entry::Entry::Occupied(entry) =
            self.operation_locks.entry(self.host_id.clone())
        {
            if Weak::ptr_eq(entry.get(), &self.operation_lock) && entry.get().strong_count() == 0 {
                entry.remove();
            }
        }
    }
}

impl RuntimeHostManager {
    pub fn new() -> Self {
        Self::with_heartbeat_timeout(DEFAULT_HEARTBEAT_TIMEOUT_SECS)
    }

    pub fn with_heartbeat_timeout(heartbeat_timeout_secs: i64) -> Self {
        Self {
            hosts: DashMap::new(),
            operation_locks: Arc::new(DashMap::new()),
            heartbeat_timeout_secs,
        }
    }

    /// Serializes lifecycle validation with every host-owned durable operation.
    ///
    /// The lock entry intentionally outlives deregistration. Reusing a host ID
    /// must retain the same ordering boundary as requests that were queued
    /// before the previous registration was removed.
    pub async fn lock_operation(&self, host_id: &str) -> RuntimeHostOperationGuard {
        let operation_lock = self.operation_lock(host_id);
        let operation_lock_weak = Arc::downgrade(&operation_lock);
        let guard = operation_lock.lock_owned().await;
        RuntimeHostOperationGuard {
            guard: Some(guard),
            operation_locks: self.operation_locks.clone(),
            host_id: host_id.to_string(),
            operation_lock: operation_lock_weak,
        }
    }

    fn operation_lock(&self, host_id: &str) -> Arc<Mutex<()>> {
        let mut entry = self.operation_locks.entry(host_id.to_string()).or_default();
        if let Some(operation_lock) = entry.upgrade() {
            return operation_lock;
        }
        let operation_lock = Arc::new(Mutex::new(()));
        *entry = Arc::downgrade(&operation_lock);
        operation_lock
    }

    #[cfg(test)]
    pub(crate) fn operation_lock_count(&self) -> usize {
        self.operation_locks.len()
    }

    pub fn register(
        &self,
        host_id: String,
        display_name: Option<String>,
        capabilities: Vec<String>,
    ) -> RuntimeHostInfo {
        let now = Utc::now();
        let lifecycle = self
            .hosts
            .get(&host_id)
            .map(|record| record.lifecycle)
            .unwrap_or_default();
        let record = RuntimeHostRecord {
            id: host_id.clone(),
            registration_id: Uuid::new_v4(),
            display_name: display_name.unwrap_or_else(|| host_id.clone()),
            capabilities,
            registered_at: now,
            last_heartbeat_at: now,
            lifecycle,
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
        self.hosts.remove(host_id).is_some()
    }

    pub fn mark_draining(&self, host_id: &str) -> Option<RuntimeHostLifecycle> {
        let mut host = self.hosts.get_mut(host_id)?;
        let previous = host.lifecycle;
        host.lifecycle = RuntimeHostLifecycle::Draining;
        Some(previous)
    }

    pub fn set_lifecycle(&self, host_id: &str, lifecycle: RuntimeHostLifecycle) -> bool {
        let Some(mut host) = self.hosts.get_mut(host_id) else {
            return false;
        };
        host.lifecycle = lifecycle;
        true
    }

    pub fn is_active(&self, host_id: &str) -> bool {
        self.hosts
            .get(host_id)
            .is_some_and(|host| host.lifecycle == RuntimeHostLifecycle::Active)
    }

    pub fn lifecycle(&self, host_id: &str) -> Option<RuntimeHostLifecycle> {
        self.hosts.get(host_id).map(|host| host.lifecycle)
    }

    pub fn active_registration_id(&self, host_id: &str) -> Option<Uuid> {
        self.hosts.get(host_id).and_then(|host| {
            (host.lifecycle == RuntimeHostLifecycle::Active).then_some(host.registration_id)
        })
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

    fn to_info(&self, record: &RuntimeHostRecord, now: DateTime<Utc>) -> RuntimeHostInfo {
        let online = (now - record.last_heartbeat_at).num_seconds() <= self.heartbeat_timeout_secs;
        RuntimeHostInfo {
            id: record.id.clone(),
            display_name: record.display_name.clone(),
            capabilities: record.capabilities.clone(),
            registered_at: record.registered_at.to_rfc3339(),
            last_heartbeat_at: record.last_heartbeat_at.to_rfc3339(),
            online,
            lifecycle: record.lifecycle,
        }
    }
}
