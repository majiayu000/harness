use crate::runtime_hosts_state::PersistedRuntimeHost;
use crate::runtime_project_cache_state::PersistedHostProjectCache;
use chrono::{DateTime, Utc};
use harness_core::db::{Migration, PgStoreContext};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use std::path::Path;

const SNAPSHOT_ID: &str = "runtime-state";
const SNAPSHOT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStateSnapshot {
    pub id: String,
    pub schema_version: u32,
    pub updated_at: DateTime<Utc>,
    pub hosts: Vec<PersistedRuntimeHost>,
    pub project_caches: Vec<PersistedHostProjectCache>,
}

impl RuntimeStateSnapshot {
    fn new(
        hosts: Vec<PersistedRuntimeHost>,
        project_caches: Vec<PersistedHostProjectCache>,
    ) -> Self {
        Self {
            id: SNAPSHOT_ID.to_string(),
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            updated_at: Utc::now(),
            hosts,
            project_caches,
        }
    }
}

static RUNTIME_STATE_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    description: "create runtime_state table",
    sql: "CREATE TABLE IF NOT EXISTS runtime_state (
        id         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
        updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
    )",
}];

pub struct RuntimeStateStore {
    pool: PgPool,
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
        Self::open_with_database_url(path, None).await
    }

    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let pool = PgStoreContext::from_path(path, configured_database_url)?
            .open_migrated_pool(RUNTIME_STATE_MIGRATIONS)
            .await?;
        Ok(Self { pool })
    }

    pub async fn load_snapshot(&self) -> anyhow::Result<Option<RuntimeStateSnapshot>> {
        Ok(self.try_load_snapshot().await?.0)
    }

    pub async fn try_load_snapshot(
        &self,
    ) -> anyhow::Result<(Option<RuntimeStateSnapshot>, LoadSnapshotOutcome)> {
        let row: Option<(String,)> = sqlx::query_as("SELECT data FROM runtime_state WHERE id = $1")
            .bind(SNAPSHOT_ID)
            .fetch_optional(&self.pool)
            .await?;

        let snapshot: RuntimeStateSnapshot = match row {
            Some((data,)) => serde_json::from_str(&data)?,
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
        project_caches: Vec<PersistedHostProjectCache>,
    ) -> anyhow::Result<()> {
        let snapshot = RuntimeStateSnapshot::new(hosts, project_caches);
        let data = serde_json::to_string(&snapshot)?;
        sqlx::query(
            "INSERT INTO runtime_state (id, data) VALUES ($1, $2)
             ON CONFLICT(id) DO UPDATE SET data = EXCLUDED.data,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(SNAPSHOT_ID)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Expose the pool for test helpers that need direct SQL access.
    #[cfg(test)]
    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::db::resolve_database_url;

    async fn open_test_store() -> anyhow::Result<Option<RuntimeStateStore>> {
        if resolve_database_url(None).is_err() {
            return Ok(None);
        }
        let dir = tempfile::tempdir()?;
        let store = RuntimeStateStore::open(&dir.path().join("runtime_state.db")).await?;
        Ok(Some(store))
    }

    #[tokio::test]
    async fn runtime_state_store_roundtrip() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        store.persist_snapshot(vec![], vec![]).await?;

        let snapshot = store.load_snapshot().await?.expect("snapshot should exist");
        assert_eq!(snapshot.id, SNAPSHOT_ID);
        assert_eq!(snapshot.schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert!(snapshot.hosts.is_empty());
        assert!(snapshot.project_caches.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn runtime_state_store_rejects_older_schema_snapshot() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        store.persist_snapshot(vec![], vec![]).await?;

        overwrite_schema_version(&store, SNAPSHOT_SCHEMA_VERSION - 1).await?;

        let snapshot = store.load_snapshot().await?;
        assert!(snapshot.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn runtime_state_store_rejects_newer_schema_snapshot_after_reopen() -> anyhow::Result<()>
    {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("runtime_state.db");

        {
            let store = RuntimeStateStore::open(&db_path).await?;
            store.persist_snapshot(vec![], vec![]).await?;
            overwrite_schema_version(&store, SNAPSHOT_SCHEMA_VERSION + 1).await?;
        }

        let reopened = RuntimeStateStore::open(&db_path).await?;
        let snapshot = reopened.load_snapshot().await?;
        assert!(snapshot.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn runtime_state_store_survives_reopen() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("runtime_state.db");

        {
            let store = RuntimeStateStore::open(&db_path).await?;
            store.persist_snapshot(vec![], vec![]).await?;
        }

        let reopened = RuntimeStateStore::open(&db_path).await?;
        let snapshot = reopened.load_snapshot().await?;
        assert!(snapshot.is_some());
        Ok(())
    }

    async fn overwrite_schema_version(
        store: &RuntimeStateStore,
        schema_version: u32,
    ) -> anyhow::Result<()> {
        let row: Option<(String,)> = sqlx::query_as("SELECT data FROM runtime_state WHERE id = $1")
            .bind(SNAPSHOT_ID)
            .fetch_optional(store.pool())
            .await?;
        let data = row.ok_or_else(|| anyhow::anyhow!("snapshot not found"))?.0;
        let mut snapshot: RuntimeStateSnapshot = serde_json::from_str(&data)?;
        snapshot.schema_version = schema_version;
        let updated_data = serde_json::to_string(&snapshot)?;
        sqlx::query("UPDATE runtime_state SET data = $1 WHERE id = $2")
            .bind(updated_data)
            .bind(SNAPSHOT_ID)
            .execute(store.pool())
            .await?;
        Ok(())
    }
}
