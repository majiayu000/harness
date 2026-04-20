use anyhow::Context;
use harness_core::db::{
    pg_create_schema_if_not_exists, pg_open_pool, pg_open_pool_schematized, Migration, PgMigrator,
};
use harness_core::{types::Thread, types::ThreadId, types::ThreadStatus};
use sqlx::postgres::PgPool;
use std::path::Path;

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
}

impl ThreadDb {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| anyhow::anyhow!("DATABASE_URL environment variable is not set"))?;
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        let schema = format!("h{:016x}", hasher.finish());

        let setup = pg_open_pool(&database_url).await?;
        pg_create_schema_if_not_exists(&setup, &schema).await?;
        setup.close().await;

        let pool = pg_open_pool_schematized(&database_url, &schema).await?;
        PgMigrator::new(&pool, THREAD_MIGRATIONS).run().await?;
        Ok(Self { pool })
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
    use std::path::PathBuf;

    async fn open_test_db() -> anyhow::Result<Option<ThreadDb>> {
        if std::env::var("DATABASE_URL").is_err() {
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
        if std::env::var("DATABASE_URL").is_err() {
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
}
