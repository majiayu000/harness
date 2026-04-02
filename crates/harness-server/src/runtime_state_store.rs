use crate::runtime_hosts_state::{PersistedRuntimeHost, PersistedTaskLease};
use crate::runtime_project_cache_state::PersistedHostProjectCache;
use chrono::{DateTime, Utc};
use harness_core::db::{Db, DbEntity};
use serde::{Deserialize, Serialize};
use std::path::Path;

const SNAPSHOT_ID: &str = "runtime-state";
const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStateSnapshot {
    pub id: String,
    pub schema_version: u32,
    pub updated_at: DateTime<Utc>,
    pub hosts: Vec<PersistedRuntimeHost>,
    pub leases: Vec<PersistedTaskLease>,
    pub project_caches: Vec<PersistedHostProjectCache>,
}

impl RuntimeStateSnapshot {
    fn new(
        hosts: Vec<PersistedRuntimeHost>,
        leases: Vec<PersistedTaskLease>,
        project_caches: Vec<PersistedHostProjectCache>,
    ) -> Self {
        Self {
            id: SNAPSHOT_ID.to_string(),
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            updated_at: Utc::now(),
            hosts,
            leases,
            project_caches,
        }
    }
}

impl DbEntity for RuntimeStateSnapshot {
    fn table_name() -> &'static str {
        "runtime_state"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn create_table_sql() -> &'static str {
        "CREATE TABLE IF NOT EXISTS runtime_state (
            id         TEXT PRIMARY KEY,
            data       TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        )"
    }
}

pub struct RuntimeStateStore {
    inner: Db<RuntimeStateSnapshot>,
}

impl RuntimeStateStore {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            inner: Db::open(path).await?,
        })
    }

    pub async fn load_snapshot(&self) -> anyhow::Result<Option<RuntimeStateSnapshot>> {
        self.inner.get(SNAPSHOT_ID).await
    }

    pub async fn persist_snapshot(
        &self,
        hosts: Vec<PersistedRuntimeHost>,
        leases: Vec<PersistedTaskLease>,
        project_caches: Vec<PersistedHostProjectCache>,
    ) -> anyhow::Result<()> {
        let snapshot = RuntimeStateSnapshot::new(hosts, leases, project_caches);
        self.inner.upsert(&snapshot).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_state_store_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = RuntimeStateStore::open(&dir.path().join("runtime_state.db")).await?;
        store.persist_snapshot(vec![], vec![], vec![]).await?;

        let snapshot = store.load_snapshot().await?.expect("snapshot should exist");
        assert_eq!(snapshot.id, SNAPSHOT_ID);
        assert_eq!(snapshot.schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert!(snapshot.hosts.is_empty());
        assert!(snapshot.leases.is_empty());
        assert!(snapshot.project_caches.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn runtime_state_store_survives_reopen() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("runtime_state.db");

        {
            let store = RuntimeStateStore::open(&db_path).await?;
            store.persist_snapshot(vec![], vec![], vec![]).await?;
        }

        let reopened = RuntimeStateStore::open(&db_path).await?;
        let snapshot = reopened.load_snapshot().await?;
        assert!(snapshot.is_some());
        Ok(())
    }
}
