use anyhow::Context;
use harness_core::db::{Migration, PgStoreContext};
use harness_core::store_backend::{PostgresBackend, StoreLocation};
use harness_core::{types::Thread, types::ThreadId, types::ThreadStatus};
use sqlx::postgres::PgPool;
use std::path::Path;

pub const THREAD_DB_SCHEMA: &str = "thread_db";

static THREAD_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create threads table",
        sql: "CREATE TABLE IF NOT EXISTS threads (
            id          TEXT PRIMARY KEY,
            cwd         TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'idle',
            turns       TEXT NOT NULL DEFAULT '[]',
            metadata    TEXT NOT NULL DEFAULT '{}',
            created_at  TIMESTAMPTZ NOT NULL,
            updated_at  TIMESTAMPTZ NOT NULL
        )",
    },
    Migration {
        version: 2,
        description: "record legacy thread backfills",
        sql: "CREATE TABLE IF NOT EXISTS thread_db_legacy_backfills (
            legacy_schema TEXT PRIMARY KEY,
            copied_rows   BIGINT NOT NULL DEFAULT 0,
            backfilled_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 3,
        description: "scope threads within shared schema",
        sql: "ALTER TABLE threads ADD COLUMN IF NOT EXISTS store_scope TEXT NOT NULL DEFAULT '';
              CREATE INDEX IF NOT EXISTS idx_threads_store_scope_created_at
              ON threads(store_scope, created_at DESC)",
    },
    Migration {
        version: 4,
        description: "promote thread scope into composite store key",
        sql: "ALTER TABLE threads ADD COLUMN IF NOT EXISTS store_scope TEXT;
              ALTER TABLE threads ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE threads
              SET store_key = COALESCE(NULLIF(store_key, ''), NULLIF(store_scope, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE threads ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE threads ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE threads DROP CONSTRAINT IF EXISTS threads_pkey;
              ALTER TABLE threads ADD CONSTRAINT threads_pkey PRIMARY KEY (store_key, id);
              CREATE INDEX IF NOT EXISTS idx_threads_store_created_at
              ON threads (store_key, created_at DESC)",
    },
];

#[derive(Clone)]
pub struct ThreadDb {
    pool: PgPool,
    schema: String,
    store_key: String,
}

impl ThreadDb {
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
        let pool = context.open_migrated_pool(THREAD_MIGRATIONS).await?;
        Ok(Self {
            pool,
            schema,
            store_key,
        })
    }

    pub fn shared_schema_context(
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<PgStoreContext> {
        PostgresBackend::new(configured_database_url.map(ToOwned::to_owned))
            .store_context(&StoreLocation::SharedSchema(THREAD_DB_SCHEMA.to_string()))
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
            .open_migrated_pool_with_setup_pool(setup_pool, THREAD_MIGRATIONS)
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

    pub async fn insert(&self, thread: &Thread) -> anyhow::Result<()> {
        let turns_json = serde_json::to_string(&thread.turns)?;
        let metadata_json = serde_json::to_string(&thread.metadata)?;
        sqlx::query(
            "INSERT INTO threads (
                store_key, id, cwd, status, turns, metadata, created_at, updated_at
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&self.store_key)
        .bind(thread.id.as_str())
        .bind(thread.project_root.to_string_lossy().as_ref())
        .bind(thread.status.as_ref())
        .bind(&turns_json)
        .bind(&metadata_json)
        .bind(thread.created_at)
        .bind(thread.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update(&self, thread: &Thread) -> anyhow::Result<()> {
        let turns_json = serde_json::to_string(&thread.turns)?;
        let metadata_json = serde_json::to_string(&thread.metadata)?;
        sqlx::query(
            "UPDATE threads SET cwd = $1, status = $2, turns = $3, metadata = $4, updated_at = $5
             WHERE store_key = $6 AND id = $7",
        )
        .bind(thread.project_root.to_string_lossy().as_ref())
        .bind(thread.status.as_ref())
        .bind(&turns_json)
        .bind(&metadata_json)
        .bind(thread.updated_at)
        .bind(&self.store_key)
        .bind(thread.id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<Thread>> {
        let row = sqlx::query_as::<_, ThreadRow>(
            "SELECT * FROM threads WHERE store_key = $1 AND id = $2",
        )
        .bind(&self.store_key)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| r.into_thread()).transpose()
    }

    pub async fn list(&self) -> anyhow::Result<Vec<Thread>> {
        let rows = sqlx::query_as::<_, ThreadRow>(
            "SELECT * FROM threads WHERE store_key = $1 ORDER BY created_at DESC",
        )
        .bind(&self.store_key)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|r| r.into_thread()).collect()
    }

    pub async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM threads WHERE store_key = $1 AND id = $2")
            .bind(&self.store_key)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

pub async fn migrate_legacy_thread_db_if_needed(
    legacy_path: &Path,
    configured_database_url: Option<&str>,
    target_db: &ThreadDb,
) -> anyhow::Result<u64> {
    let legacy_context =
        PgStoreContext::from_legacy_path_schema(legacy_path, configured_database_url)?;
    let legacy_schema = legacy_context.schema();
    if legacy_schema == target_db.schema() {
        return Ok(0);
    }

    let already_backfilled: Option<String> = sqlx::query_scalar(
        "SELECT legacy_schema FROM thread_db_legacy_backfills WHERE legacy_schema = $1",
    )
    .bind(legacy_schema)
    .fetch_optional(&target_db.pool)
    .await?;
    if already_backfilled.is_some() {
        return Ok(0);
    }

    let legacy_table: Option<String> = sqlx::query_scalar(
        "SELECT table_name::text
         FROM information_schema.tables
         WHERE table_schema = $1 AND table_name = 'threads'",
    )
    .bind(legacy_schema)
    .fetch_optional(&target_db.pool)
    .await?;
    if legacy_table.is_none() {
        return Ok(0);
    }

    let copy_sql = format!(
        "INSERT INTO threads (
            store_key, id, cwd, status, turns, metadata, created_at, updated_at
         )
         SELECT $1, id, cwd, status, turns, metadata, created_at, updated_at
         FROM \"{legacy_schema}\".threads
         ON CONFLICT (store_key, id) DO NOTHING"
    );
    let copied = sqlx::query(&copy_sql)
        .bind(&target_db.store_key)
        .execute(&target_db.pool)
        .await?
        .rows_affected();
    sqlx::query(
        "INSERT INTO thread_db_legacy_backfills (legacy_schema, copied_rows)
         VALUES ($1, $2)
         ON CONFLICT (legacy_schema) DO NOTHING",
    )
    .bind(legacy_schema)
    .bind(copied as i64)
    .execute(&target_db.pool)
    .await?;

    if copied > 0 {
        tracing::info!(
            copied,
            legacy_schema,
            target_schema = target_db.schema(),
            "thread db migration: backfilled legacy threads into shared schema"
        );
    }

    Ok(copied)
}

#[derive(sqlx::FromRow)]
struct ThreadRow {
    store_key: String,
    id: String,
    cwd: String,
    status: String,
    turns: String,
    metadata: String,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl ThreadRow {
    fn into_thread(self) -> anyhow::Result<Thread> {
        let _store_key = self.store_key;
        let id = ThreadId::from_str(&self.id);
        let status = self
            .status
            .parse::<ThreadStatus>()
            .with_context(|| format!("invalid status for thread `{}`", id.as_str()))?;
        Ok(Thread {
            id,
            project_root: std::path::PathBuf::from(self.cwd),
            status,
            turns: serde_json::from_str(&self.turns)?,
            metadata: serde_json::from_str(&self.metadata)?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use harness_core::db::{pg_open_pool, resolve_test_database_url, TestSchemaGuard};
    use std::path::PathBuf;

    async fn open_test_db() -> anyhow::Result<Option<ThreadDb>> {
        if resolve_test_database_url(None).is_err() {
            return Ok(None);
        }
        let dir = tempfile::tempdir()?;
        let db = ThreadDb::open(&dir.path().join("threads.db")).await?;
        Ok(Some(db))
    }

    #[tokio::test]
    async fn thread_db_roundtrip() -> anyhow::Result<()> {
        let Some(db) = open_test_db().await? else {
            return Ok(());
        };

        let thread = Thread::new(PathBuf::from("/tmp/project"));
        db.insert(&thread).await?;

        let loaded = db.get(thread.id.as_str()).await?;
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.id, thread.id);
        assert_eq!(loaded.project_root, thread.project_root);
        assert_eq!(loaded.status, ThreadStatus::Idle);
        Ok(())
    }

    #[tokio::test]
    async fn thread_db_list_and_delete() -> anyhow::Result<()> {
        let Some(db) = open_test_db().await? else {
            return Ok(());
        };

        let t1 = Thread::new(PathBuf::from("/a"));
        let t2 = Thread::new(PathBuf::from("/b"));
        db.insert(&t1).await?;
        db.insert(&t2).await?;

        let all = db.list().await?;
        assert_eq!(all.len(), 2);

        let deleted = db.delete(t1.id.as_str()).await?;
        assert!(deleted);
        let all = db.list().await?;
        assert_eq!(all.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn thread_db_survives_reopen() -> anyhow::Result<()> {
        if resolve_test_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("threads.db");

        let thread_id;
        {
            let db = ThreadDb::open(&db_path).await?;
            let thread = Thread::new(PathBuf::from("/srv/app"));
            thread_id = thread.id.clone();
            db.insert(&thread).await?;
        }

        let db = ThreadDb::open(&db_path).await?;
        let loaded = db.get(thread_id.as_str()).await?;
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().project_root, PathBuf::from("/srv/app"));
        Ok(())
    }

    #[tokio::test]
    async fn thread_db_rejects_unknown_status() -> anyhow::Result<()> {
        let Some(db) = open_test_db().await? else {
            return Ok(());
        };

        let thread = Thread::new(PathBuf::from("/srv/app"));
        db.insert(&thread).await?;

        sqlx::query("UPDATE threads SET status = $1 WHERE id = $2")
            .bind("paused")
            .bind(thread.id.as_str())
            .execute(&db.pool)
            .await?;

        let err = db
            .get(thread.id.as_str())
            .await
            .expect_err("unknown status must return an explicit error");
        let message = format!("{err:#}");
        assert!(message.contains("invalid status for thread"));
        assert!(message.contains("unknown thread status `paused`"));
        Ok(())
    }

    #[tokio::test]
    async fn thread_db_update_roundtrip() -> anyhow::Result<()> {
        let Some(db) = open_test_db().await? else {
            return Ok(());
        };

        let mut thread = Thread::new(PathBuf::from("/original"));
        db.insert(&thread).await?;

        thread.status = ThreadStatus::Active;
        thread.project_root = PathBuf::from("/updated");
        thread.updated_at = chrono::Utc::now();
        db.update(&thread).await?;

        let loaded = db
            .get(thread.id.as_str())
            .await?
            .expect("updated thread should exist");
        assert_eq!(loaded.status, ThreadStatus::Active);
        assert_eq!(loaded.project_root, PathBuf::from("/updated"));
        Ok(())
    }

    #[tokio::test]
    async fn thread_db_metadata_persists() -> anyhow::Result<()> {
        let Some(db) = open_test_db().await? else {
            return Ok(());
        };

        let mut thread = Thread::new(PathBuf::from("/meta"));
        thread.metadata = serde_json::json!({"key": "value", "count": 42});
        db.insert(&thread).await?;

        let loaded = db
            .get(thread.id.as_str())
            .await?
            .expect("thread with metadata should exist");
        assert_eq!(loaded.metadata["key"], "value");
        assert_eq!(loaded.metadata["count"], 42);
        Ok(())
    }

    #[tokio::test]
    async fn thread_db_delete_nonexistent_returns_false() -> anyhow::Result<()> {
        let Some(db) = open_test_db().await? else {
            return Ok(());
        };

        let deleted = db.delete("does-not-exist").await?;
        assert!(!deleted);
        Ok(())
    }

    #[tokio::test]
    async fn thread_db_get_returns_none_for_missing() -> anyhow::Result<()> {
        let Some(db) = open_test_db().await? else {
            return Ok(());
        };

        assert!(db.get("missing-id").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn shared_schema_thread_db_keeps_store_rows_isolated() -> anyhow::Result<()> {
        let database_url = match resolve_test_database_url(None) {
            Ok(url) => url,
            Err(_) => return Ok(()),
        };
        let dir = tempfile::tempdir()?;
        let setup_pool = pg_open_pool(&database_url).await?;
        let mut shared_schema = TestSchemaGuard::new(&database_url, "thread_db_shared_scope_test")?;
        let shared_context =
            PgStoreContext::from_schema(shared_schema.schema(), Some(&database_url))?;
        let store_a_dir = dir.path().join("store-a");
        let store_b_dir = dir.path().join("store-b");
        let store_a =
            ThreadDb::open_shared_with_data_dir(&shared_context, &setup_pool, &store_a_dir).await?;
        let store_b =
            ThreadDb::open_shared_with_data_dir(&shared_context, &setup_pool, &store_b_dir).await?;

        let result = std::panic::AssertUnwindSafe(async {
            assert_ne!(store_a.store_key(), store_b.store_key());

            let mut thread_a = Thread::new(PathBuf::from("/project-a"));
            thread_a.id = ThreadId::from_str("same-thread-id");
            let mut thread_b = thread_a.clone();
            thread_b.project_root = PathBuf::from("/project-b");

            store_a.insert(&thread_a).await?;
            store_b.insert(&thread_b).await?;

            assert_eq!(store_a.list().await?.len(), 1);
            assert_eq!(store_b.list().await?.len(), 1);
            assert_eq!(
                store_a
                    .get("same-thread-id")
                    .await?
                    .expect("store a thread should exist")
                    .project_root,
                PathBuf::from("/project-a")
            );
            assert_eq!(
                store_b
                    .get("same-thread-id")
                    .await?
                    .expect("store b thread should exist")
                    .project_root,
                PathBuf::from("/project-b")
            );

            let mut wrong_store_update = thread_a.clone();
            wrong_store_update.project_root = PathBuf::from("/wrong-store-update");
            store_b.update(&wrong_store_update).await?;
            assert_eq!(
                store_a
                    .get("same-thread-id")
                    .await?
                    .expect("store a thread should remain")
                    .project_root,
                PathBuf::from("/project-a"),
                "updates must be scoped by store key"
            );

            assert!(!store_b.delete("missing-thread-id").await?);
            assert!(store_b.delete("same-thread-id").await?);
            assert!(
                store_a.get("same-thread-id").await?.is_some(),
                "deletes must not cross store keys"
            );
            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        store_a.pool.close().await;
        store_b.pool.close().await;
        let cleanup_result = shared_schema.cleanup_with_pool(&setup_pool).await;
        setup_pool.close().await;

        match result {
            Ok(result) => {
                cleanup_result?;
                result
            }
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[test]
    fn shared_schema_context_uses_fixed_thread_db_schema() -> anyhow::Result<()> {
        let context =
            ThreadDb::shared_schema_context(Some("postgres://user:pass@localhost:5432/harness"))?;
        assert_eq!(context.schema(), THREAD_DB_SCHEMA);
        assert!(
            context.ownership().is_none(),
            "shared thread_db schema must not register path-derived ownership"
        );
        Ok(())
    }

    #[tokio::test]
    async fn legacy_thread_db_migration_backfills_shared_schema() -> anyhow::Result<()> {
        let database_url = match resolve_test_database_url(None) {
            Ok(url) => url,
            Err(_) => return Ok(()),
        };
        let dir = tempfile::tempdir()?;
        let target_data_dir = dir.path().join("target-data");
        let other_data_dir = dir.path().join("other-data");
        let legacy_path = target_data_dir.join("threads.db");
        let legacy_schema =
            PgStoreContext::from_legacy_path_schema(&legacy_path, Some(&database_url))?
                .schema()
                .to_owned();
        let setup_pool = pg_open_pool(&database_url).await?;
        let mut target_schema = TestSchemaGuard::new(&database_url, "thread_db_test")?;
        let target_context =
            PgStoreContext::from_schema(target_schema.schema(), Some(&database_url))?;
        let target_db =
            ThreadDb::open_shared_with_data_dir(&target_context, &setup_pool, &target_data_dir)
                .await?;
        let other_scope_db =
            ThreadDb::open_shared_with_data_dir(&target_context, &setup_pool, &other_data_dir)
                .await?;
        let legacy_db = ThreadDb::open_with_database_url(&legacy_path, Some(&database_url)).await?;

        let result = std::panic::AssertUnwindSafe(async {
            let thread = Thread::new(PathBuf::from("/legacy/project"));
            let thread_id = thread.id.clone();
            legacy_db.insert(&thread).await?;

            let copied =
                migrate_legacy_thread_db_if_needed(&legacy_path, Some(&database_url), &target_db)
                    .await?;
            assert_eq!(copied, 1, "one legacy thread should be copied");

            let copied_again =
                migrate_legacy_thread_db_if_needed(&legacy_path, Some(&database_url), &target_db)
                    .await?;
            assert_eq!(copied_again, 0, "migration must be idempotent");

            let loaded = target_db
                .get(thread_id.as_str())
                .await?
                .expect("legacy thread should be present in the shared schema");
            assert_eq!(loaded.project_root, PathBuf::from("/legacy/project"));
            assert!(
                other_scope_db.get(thread_id.as_str()).await?.is_none(),
                "other data_dir scopes must not hydrate legacy threads"
            );
            assert!(
                !other_scope_db.delete(thread_id.as_str()).await?,
                "other data_dir scopes must not delete backfilled threads"
            );

            assert!(target_db.delete(thread_id.as_str()).await?);
            let copied_after_delete =
                migrate_legacy_thread_db_if_needed(&legacy_path, Some(&database_url), &target_db)
                    .await?;
            assert_eq!(
                copied_after_delete, 0,
                "completed migration must not recopy legacy threads"
            );
            assert!(
                target_db.get(thread_id.as_str()).await?.is_none(),
                "completed migration must not resurrect deleted shared threads"
            );
            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        legacy_db.pool.close().await;
        target_db.pool.close().await;
        other_scope_db.pool.close().await;
        let _ = sqlx::query(&format!(
            "DROP SCHEMA IF EXISTS \"{legacy_schema}\" CASCADE"
        ))
        .execute(&setup_pool)
        .await;
        let cleanup_result = target_schema.cleanup_with_pool(&setup_pool).await;
        setup_pool.close().await;

        match result {
            Ok(result) => {
                cleanup_result?;
                result
            }
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}
