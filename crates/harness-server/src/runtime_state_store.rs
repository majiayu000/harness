use crate::runtime_hosts_state::PersistedRuntimeHost;
use crate::runtime_project_cache_state::PersistedHostProjectCache;
use chrono::{DateTime, Utc};
use harness_core::db::{Migration, PgStoreContext};
use harness_core::store_backend::{PostgresBackend, StoreLocation};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use std::path::Path;

const SNAPSHOT_ID: &str = "runtime-state";
const SNAPSHOT_SCHEMA_VERSION: u32 = 2;
pub const RUNTIME_STATE_STORE_SCHEMA: &str = "runtime_state_store";

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

static RUNTIME_STATE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create runtime_state table",
        sql: "CREATE TABLE IF NOT EXISTS runtime_state (
            id         TEXT PRIMARY KEY,
            data       TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 2,
        description: "record legacy runtime state backfills",
        sql: "CREATE TABLE IF NOT EXISTS runtime_state_store_legacy_backfills (
            store_key     TEXT NOT NULL DEFAULT current_schema(),
            legacy_schema TEXT NOT NULL,
            copied_rows   BIGINT NOT NULL DEFAULT 0,
            backfilled_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (store_key, legacy_schema)
        )",
    },
    Migration {
        version: 3,
        description: "scope runtime state within shared schema",
        sql: "ALTER TABLE runtime_state ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE runtime_state
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE runtime_state ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE runtime_state ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE runtime_state DROP CONSTRAINT IF EXISTS runtime_state_pkey;
              ALTER TABLE runtime_state ADD CONSTRAINT runtime_state_pkey PRIMARY KEY (store_key, id);
              CREATE INDEX IF NOT EXISTS idx_runtime_state_store_updated_at
              ON runtime_state (store_key, updated_at DESC)",
    },
    Migration {
        version: 4,
        description: "scope legacy runtime state backfill markers",
        sql: "ALTER TABLE runtime_state_store_legacy_backfills
              ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE runtime_state_store_legacy_backfills
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE runtime_state_store_legacy_backfills
              ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE runtime_state_store_legacy_backfills
              ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE runtime_state_store_legacy_backfills
              DROP CONSTRAINT IF EXISTS runtime_state_store_legacy_backfills_pkey;
              ALTER TABLE runtime_state_store_legacy_backfills
              ADD CONSTRAINT runtime_state_store_legacy_backfills_pkey
              PRIMARY KEY (store_key, legacy_schema)",
    },
];

pub struct RuntimeStateStore {
    pool: PgPool,
    schema: String,
    store_key: String,
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
        let context = PgStoreContext::from_legacy_path_schema(path, configured_database_url)?;
        let schema = context.schema().to_owned();
        let store_key = schema.clone();
        let pool = context.open_migrated_pool(RUNTIME_STATE_MIGRATIONS).await?;
        Ok(Self {
            pool,
            schema,
            store_key,
        })
    }

    pub fn shared_schema_context(
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<PgStoreContext> {
        PostgresBackend::new(configured_database_url.map(ToOwned::to_owned)).store_context(
            &StoreLocation::SharedSchema(RUNTIME_STATE_STORE_SCHEMA.to_string()),
        )
    }

    pub async fn open_with_context(
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        Self::open_with_context_and_store_key(context, setup_pool, context.schema().to_owned())
            .await
    }

    pub async fn open_shared_with_data_dir(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        data_dir: &Path,
    ) -> anyhow::Result<Self> {
        let store_key = Self::store_key_for_data_dir(data_dir);
        Self::open_with_context_and_store_key(context, setup_pool, store_key).await
    }

    async fn open_with_context_and_store_key(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        store_key: String,
    ) -> anyhow::Result<Self> {
        let schema = context.schema().to_owned();
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, RUNTIME_STATE_MIGRATIONS)
            .await?;
        Ok(Self {
            pool,
            schema,
            store_key,
        })
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn store_key_for_data_dir(data_dir: &Path) -> String {
        data_dir
            .canonicalize()
            .unwrap_or_else(|_| data_dir.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }

    pub fn store_key(&self) -> &str {
        &self.store_key
    }

    pub async fn load_snapshot(&self) -> anyhow::Result<Option<RuntimeStateSnapshot>> {
        Ok(self.try_load_snapshot().await?.0)
    }

    pub async fn try_load_snapshot(
        &self,
    ) -> anyhow::Result<(Option<RuntimeStateSnapshot>, LoadSnapshotOutcome)> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data FROM runtime_state WHERE store_key = $1 AND id = $2")
                .bind(&self.store_key)
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
            "INSERT INTO runtime_state (store_key, id, data) VALUES ($1, $2, $3)
             ON CONFLICT(store_key, id) DO UPDATE SET data = EXCLUDED.data,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&self.store_key)
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

pub async fn migrate_legacy_runtime_state_store_if_needed(
    legacy_path: &Path,
    configured_database_url: Option<&str>,
    target_store: &RuntimeStateStore,
) -> anyhow::Result<u64> {
    let legacy_context =
        PgStoreContext::from_legacy_path_schema(legacy_path, configured_database_url)?;
    let legacy_schema = legacy_context.schema();
    if legacy_schema == target_store.schema() {
        return Ok(0);
    }

    let already_backfilled: Option<String> = sqlx::query_scalar(
        "SELECT legacy_schema
         FROM runtime_state_store_legacy_backfills
         WHERE store_key = $1 AND legacy_schema = $2",
    )
    .bind(target_store.store_key())
    .bind(legacy_schema)
    .fetch_optional(&target_store.pool)
    .await?;
    if already_backfilled.is_some() {
        return Ok(0);
    }

    let legacy_table: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("\"{legacy_schema}\".runtime_state"))
        .fetch_one(&target_store.pool)
        .await?;
    if legacy_table.is_none() {
        return Ok(0);
    }

    let mut tx = target_store.pool.begin().await?;

    let copy_sql = format!(
        "INSERT INTO runtime_state (store_key, id, data, created_at, updated_at)
         SELECT $1, id, data, created_at, updated_at
         FROM \"{legacy_schema}\".runtime_state
         ON CONFLICT (store_key, id) DO NOTHING"
    );
    let copied = sqlx::query(&copy_sql)
        .bind(target_store.store_key())
        .execute(&mut *tx)
        .await?
        .rows_affected();

    sqlx::query(
        "INSERT INTO runtime_state_store_legacy_backfills (store_key, legacy_schema, copied_rows)
         VALUES ($1, $2, $3)
         ON CONFLICT (store_key, legacy_schema) DO NOTHING",
    )
    .bind(target_store.store_key())
    .bind(legacy_schema)
    .bind(copied as i64)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    if copied > 0 {
        tracing::info!(
            copied,
            legacy_schema,
            target_schema = target_store.schema(),
            "runtime state migration: backfilled legacy snapshot into shared schema"
        );
    }

    Ok(copied)
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
