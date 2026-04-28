use crate::runtime_hosts::{RuntimeHostManager, RuntimeHostRecord};
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

impl RuntimeHostManager {
    pub fn snapshot_state(&self) -> Vec<PersistedRuntimeHost> {
        self.hosts
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
            .collect()
    }

    pub fn restore_state(&self, hosts: Vec<PersistedRuntimeHost>) -> usize {
        self.hosts.clear();

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
        self.hosts.len()
    }
}
