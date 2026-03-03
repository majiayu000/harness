use harness_core::{Thread, ThreadId, ThreadStatus};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::Path;

pub struct ThreadDb {
    pool: SqlitePool,
}

impl ThreadDb {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let url = format!("sqlite:{}?mode=rwc", path.display());
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS threads (
                id TEXT PRIMARY KEY,
                cwd TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'idle',
                turns TEXT NOT NULL DEFAULT '[]',
                metadata TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert(&self, thread: &Thread) -> anyhow::Result<()> {
        let turns_json = serde_json::to_string(&thread.turns)?;
        let metadata_json = serde_json::to_string(&thread.metadata)?;
        sqlx::query(
            "INSERT INTO threads (id, cwd, status, turns, metadata, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(thread.id.as_str())
        .bind(thread.project_root.to_string_lossy().as_ref())
        .bind(status_to_str(thread.status))
        .bind(&turns_json)
        .bind(&metadata_json)
        .bind(thread.created_at.to_rfc3339())
        .bind(thread.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update(&self, thread: &Thread) -> anyhow::Result<()> {
        let turns_json = serde_json::to_string(&thread.turns)?;
        let metadata_json = serde_json::to_string(&thread.metadata)?;
        sqlx::query(
            "UPDATE threads SET cwd = ?, status = ?, turns = ?, metadata = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(thread.project_root.to_string_lossy().as_ref())
        .bind(status_to_str(thread.status))
        .bind(&turns_json)
        .bind(&metadata_json)
        .bind(thread.updated_at.to_rfc3339())
        .bind(thread.id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<Thread>> {
        let row = sqlx::query_as::<_, ThreadRow>("SELECT * FROM threads WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.into_thread()))
    }

    pub async fn list(&self) -> anyhow::Result<Vec<Thread>> {
        let rows = sqlx::query_as::<_, ThreadRow>(
            "SELECT * FROM threads ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.into_thread()).collect())
    }

    pub async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM threads WHERE id = ?")
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
    created_at: String,
    updated_at: String,
}

impl ThreadRow {
    fn into_thread(self) -> Thread {
        Thread {
            id: ThreadId::from_str(&self.id),
            project_root: std::path::PathBuf::from(self.cwd),
            status: str_to_status(&self.status),
            turns: serde_json::from_str(&self.turns).unwrap_or_default(),
            metadata: serde_json::from_str(&self.metadata)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
            created_at: chrono::DateTime::parse_from_rfc3339(&self.created_at)
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
            updated_at: chrono::DateTime::parse_from_rfc3339(&self.updated_at)
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
        }
    }
}

fn status_to_str(s: ThreadStatus) -> &'static str {
    match s {
        ThreadStatus::Idle => "idle",
        ThreadStatus::Active => "active",
        ThreadStatus::Archived => "archived",
    }
}

fn str_to_status(s: &str) -> ThreadStatus {
    match s {
        "idle" => ThreadStatus::Idle,
        "active" => ThreadStatus::Active,
        "archived" => ThreadStatus::Archived,
        _ => ThreadStatus::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn thread_db_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("threads.db");
        let db = ThreadDb::open(&db_path).await?;

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
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("threads.db");
        let db = ThreadDb::open(&db_path).await?;

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
}
