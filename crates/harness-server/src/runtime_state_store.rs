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
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )"
    }
}

pub struct RuntimeStateStore {
    inner: Db<RuntimeStateSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadSnapshotOutcome {
    NotFound,
    Loaded,
    SchemaMismatch { found: u32, expected: u32 },
}

impl LoadSnapshotOutcome {
    pub fn is_schema_mismatch(self) -> bool {
        matches!(self, Self::SchemaMismatch { .. })
    }
}

impl RuntimeStateStore {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            inner: Db::open(path).await?,
        })
    }

    pub async fn load_snapshot(&self) -> anyhow::Result<Option<RuntimeStateSnapshot>> {
        Ok(self.try_load_snapshot().await?.0)
    }

    pub async fn try_load_snapshot(
        &self,
    ) -> anyhow::Result<(Option<RuntimeStateSnapshot>, LoadSnapshotOutcome)> {
        let snapshot = match self.inner.get(SNAPSHOT_ID).await? {
            Some(snapshot) => snapshot,
            None => return Ok((None, LoadSnapshotOutcome::NotFound)),
        };

        if snapshot.schema_version != SNAPSHOT_SCHEMA_VERSION {
            tracing::warn!(
                snapshot_id = SNAPSHOT_ID,
                found_schema_version = snapshot.schema_version,
                expected_schema_version = SNAPSHOT_SCHEMA_VERSION,
                "runtime state snapshot schema mismatch; skipping restore"
            );
            return Ok((
                None,
                LoadSnapshotOutcome::SchemaMismatch {
                    found: snapshot.schema_version,
                    expected: SNAPSHOT_SCHEMA_VERSION,
                },
            ));
        }

        Ok((Some(snapshot), LoadSnapshotOutcome::Loaded))
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
    use harness_core::db::open_pool;
    use sqlx::Row;

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
    async fn runtime_state_store_rejects_older_schema_snapshot() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("runtime_state.db");
        let store = RuntimeStateStore::open(&db_path).await?;
        store.persist_snapshot(vec![], vec![], vec![]).await?;

        overwrite_schema_version(&db_path, SNAPSHOT_SCHEMA_VERSION - 1).await?;

        let snapshot = store.load_snapshot().await?;
        assert!(snapshot.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn runtime_state_store_rejects_newer_schema_snapshot_after_reopen() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("runtime_state.db");

        {
            let store = RuntimeStateStore::open(&db_path).await?;
            store.persist_snapshot(vec![], vec![], vec![]).await?;
        }

        overwrite_schema_version(&db_path, SNAPSHOT_SCHEMA_VERSION + 1).await?;

        let reopened = RuntimeStateStore::open(&db_path).await?;
        let snapshot = reopened.load_snapshot().await?;
        assert!(snapshot.is_none());
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

    async fn overwrite_schema_version(path: &Path, schema_version: u32) -> anyhow::Result<()> {
        let pool = open_pool(path).await?;
        let row = sqlx::query("SELECT data FROM runtime_state WHERE id = $1")
            .bind(SNAPSHOT_ID)
            .fetch_one(&pool)
            .await?;
        let data: String = row.try_get("data")?;
        let mut snapshot: RuntimeStateSnapshot = serde_json::from_str(&data)?;
        snapshot.schema_version = schema_version;
        let updated_data = serde_json::to_string(&snapshot)?;
        sqlx::query("UPDATE runtime_state SET data = $1 WHERE id = $2")
            .bind(updated_data)
            .bind(SNAPSHOT_ID)
            .execute(&pool)
            .await?;
        Ok(())
    }
}
