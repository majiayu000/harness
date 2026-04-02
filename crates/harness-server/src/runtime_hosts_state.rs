use crate::runtime_hosts::{RuntimeHostManager, RuntimeHostRecord, TaskLease};
use crate::task_runner::TaskId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedRuntimeHost {
    pub id: String,
    pub display_name: String,
    pub capabilities: Vec<String>,
    pub registered_at: DateTime<Utc>,
    pub last_heartbeat_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTaskLease {
    pub task_id: TaskId,
    pub host_id: String,
    pub expires_at: DateTime<Utc>,
}

impl RuntimeHostManager {
    pub fn snapshot_state(&self) -> (Vec<PersistedRuntimeHost>, Vec<PersistedTaskLease>) {
        let hosts = self
            .hosts
            .iter()
            .map(|entry| {
                let host = entry.value();
                PersistedRuntimeHost {
                    id: host.id.clone(),
                    display_name: host.display_name.clone(),
                    capabilities: host.capabilities.clone(),
                    registered_at: host.registered_at,
                    last_heartbeat_at: host.last_heartbeat_at,
                }
            })
            .collect();
        let leases = self
            .leases
            .iter()
            .map(|entry| {
                let lease = entry.value();
                PersistedTaskLease {
                    task_id: entry.key().clone(),
                    host_id: lease.host_id.clone(),
                    expires_at: lease.expires_at,
                }
            })
            .collect();
        (hosts, leases)
    }

    pub fn restore_state(
        &self,
        hosts: Vec<PersistedRuntimeHost>,
        leases: Vec<PersistedTaskLease>,
    ) -> (usize, usize) {
        self.hosts.clear();
        self.leases.clear();
        self.host_leases.clear();
        self.lease_expirations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();

        for host in hosts {
            self.hosts.insert(
                host.id.clone(),
                RuntimeHostRecord {
                    id: host.id,
                    display_name: host.display_name,
                    capabilities: host.capabilities,
                    registered_at: host.registered_at,
                    last_heartbeat_at: host.last_heartbeat_at,
                },
            );
        }

        let now = Utc::now();
        for lease in leases {
            if lease.expires_at <= now {
                continue;
            }
            if !self.hosts.contains_key(&lease.host_id) {
                continue;
            }
            self.leases.insert(
                lease.task_id.clone(),
                TaskLease {
                    host_id: lease.host_id.clone(),
                    expires_at: lease.expires_at,
                },
            );
            self.index_lease(&lease.host_id, lease.task_id, lease.expires_at);
        }
        (self.hosts.len(), self.leases.len())
    }
}
