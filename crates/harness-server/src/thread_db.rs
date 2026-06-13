use anyhow::Context;
use harness_core::db::{Migration, PgStoreContext};
use harness_core::store_backend::{PostgresBackend, StoreLocation};
use harness_core::{types::Thread, types::ThreadId, types::ThreadStatus};
use sqlx::postgres::PgPool;
use std::path::Path;

pub const THREAD_DB_SCHEMA: &str = "thread_db";

static THREAD_MIGRATIONS: &[Migration] = &[Migration {
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
}];

#[derive(Clone)]
pub struct ThreadDb {
    pool: PgPool,
    schema: String,
}

impl ThreadDb {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_database_url(path, None).await
    }

    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let context = PgStoreContext::from_path(path, configured_database_url)?;
        let schema = context.schema().to_owned();
        let pool = context.open_migrated_pool(THREAD_MIGRATIONS).await?;
        Ok(Self { pool, schema })
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
        let schema = context.schema().to_owned();
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, THREAD_MIGRATIONS)
            .await?;
        Ok(Self { pool, schema })
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub async fn insert(&self, thread: &Thread) -> anyhow::Result<()> {
        let turns_json = serde_json::to_string(&thread.turns)?;
        let metadata_json = serde_json::to_string(&thread.metadata)?;
        sqlx::query(
            "INSERT INTO threads (id, cwd, status, turns, metadata, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
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
             WHERE id = $6",
        )
        .bind(thread.project_root.to_string_lossy().as_ref())
        .bind(thread.status.as_ref())
        .bind(&turns_json)
        .bind(&metadata_json)
        .bind(thread.updated_at)
        .bind(thread.id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<Thread>> {
        let row = sqlx::query_as::<_, ThreadRow>("SELECT * FROM threads WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| r.into_thread()).transpose()
    }

    pub async fn list(&self) -> anyhow::Result<Vec<Thread>> {
        let rows = sqlx::query_as::<_, ThreadRow>("SELECT * FROM threads ORDER BY created_at DESC")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(|r| r.into_thread()).collect()
    }

    pub async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM threads WHERE id = $1")
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
    let legacy_context = PgStoreContext::from_path(legacy_path, configured_database_url)?;
    let legacy_schema = legacy_context.schema();
    if legacy_schema == target_db.schema() {
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
            id, cwd, status, turns, metadata, created_at, updated_at
         )
         SELECT id, cwd, status, turns, metadata, created_at, updated_at
         FROM \"{legacy_schema}\".threads
         ON CONFLICT (id) DO NOTHING"
    );
    let copied = sqlx::query(&copy_sql)
        .execute(&target_db.pool)
        .await?
        .rows_affected();

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
    use harness_core::db::{pg_open_pool, resolve_database_url};
    use std::path::PathBuf;

    fn unique_test_schema(prefix: &str) -> String {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_nanos();
        format!("{prefix}_{nanos}_{count}")
    }

    async fn open_test_db() -> anyhow::Result<Option<ThreadDb>> {
        if resolve_database_url(None).is_err() {
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
        if resolve_database_url(None).is_err() {
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
        let database_url = match resolve_database_url(None) {
            Ok(url) => url,
            Err(_) => return Ok(()),
        };
        let dir = tempfile::tempdir()?;
        let legacy_path = dir.path().join("threads.db");
        let legacy_schema = PgStoreContext::from_path(&legacy_path, Some(&database_url))?
            .schema()
            .to_owned();
        let target_schema = unique_test_schema("thread_db_test");
        let setup_pool = pg_open_pool(&database_url).await?;
        let target_context = PgStoreContext::from_schema(&target_schema, Some(&database_url))?;
        let target_db = ThreadDb::open_with_context(&target_context, &setup_pool).await?;
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
            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        legacy_db.pool.close().await;
        target_db.pool.close().await;
        let _ = sqlx::query(&format!(
            "DROP SCHEMA IF EXISTS \"{legacy_schema}\" CASCADE"
        ))
        .execute(&setup_pool)
        .await;
        let _ = sqlx::query(&format!(
            "DROP SCHEMA IF EXISTS \"{target_schema}\" CASCADE"
        ))
        .execute(&setup_pool)
        .await;
        setup_pool.close().await;

        match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}
