use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::Path;

/// Marker trait for entities that can be stored in a generic JSON-blob database.
pub trait DbEntity: Serialize + for<'de> Deserialize<'de> + Send + Unpin + 'static {
    /// The SQL table name for this entity type.
    fn table_name() -> &'static str;
    /// The primary key value for this entity instance.
    fn id(&self) -> &str;
    /// DDL executed once on open to ensure the table exists.
    fn create_table_sql() -> &'static str;
}

/// Create a SQLite connection pool for the given path.
///
/// All three DB files (task, thread, plan) share the same pool configuration.
pub async fn open_pool(path: &Path) -> anyhow::Result<SqlitePool> {
    let url = format!("sqlite:{}?mode=rwc", path.display());
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&url)
        .await?;
    Ok(pool)
}

/// Generic SQLite store that persists entities as JSON blobs.
///
/// Schema:
/// ```sql
/// CREATE TABLE IF NOT EXISTS <table_name> (
///     id         TEXT PRIMARY KEY,
///     data       TEXT NOT NULL,
///     created_at TEXT NOT NULL DEFAULT (datetime('now')),
///     updated_at TEXT NOT NULL DEFAULT (datetime('now'))
/// )
/// ```
///
/// Use this when entities do not need queryable columns beyond `id`.
/// For entities requiring SQL-level filtering (e.g. `WHERE status = ?`),
/// keep a specialised DB struct and call [`open_pool`] for pool creation.
pub struct Db<T: DbEntity> {
    pub(crate) pool: SqlitePool,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: DbEntity> Db<T> {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let pool = open_pool(path).await?;
        let db = Self {
            pool,
            _phantom: std::marker::PhantomData,
        };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::query(T::create_table_sql())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Insert or update an entity (upsert by id).
    pub async fn upsert(&self, entity: &T) -> anyhow::Result<()> {
        let data = serde_json::to_string(entity)?;
        let sql = format!(
            "INSERT INTO {table} (id, data) VALUES (?, ?)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data,
                 updated_at = datetime('now')",
            table = T::table_name()
        );
        sqlx::query(&sql)
            .bind(entity.id())
            .bind(&data)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<T>> {
        let sql = format!("SELECT data FROM {} WHERE id = ?", T::table_name());
        let row: Option<(String,)> = sqlx::query_as(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some((data,)) => Ok(Some(serde_json::from_str(&data)?)),
            None => Ok(None),
        }
    }

    pub async fn list(&self) -> anyhow::Result<Vec<T>> {
        let sql = format!(
            "SELECT data FROM {} ORDER BY created_at DESC",
            T::table_name()
        );
        let rows: Vec<(String,)> = sqlx::query_as(&sql).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let sql = format!("DELETE FROM {} WHERE id = ?", T::table_name());
        let result = sqlx::query(&sql).bind(id).execute(&self.pool).await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Note {
        id: String,
        body: String,
    }

    impl Note {
        fn new(id: &str, body: &str) -> Self {
            Self {
                id: id.to_string(),
                body: body.to_string(),
            }
        }
    }

    impl DbEntity for Note {
        fn table_name() -> &'static str {
            "notes"
        }

        fn id(&self) -> &str {
            &self.id
        }

        fn create_table_sql() -> &'static str {
            "CREATE TABLE IF NOT EXISTS notes (
                id         TEXT PRIMARY KEY,
                data       TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )"
        }
    }

    #[tokio::test]
    async fn upsert_and_get_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

        let note = Note::new("n1", "hello");
        db.upsert(&note).await?;

        let loaded = db
            .get("n1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("note should exist"))?;
        assert_eq!(loaded, note);
        Ok(())
    }

    #[tokio::test]
    async fn get_returns_none_for_missing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

        assert!(db.get("missing").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn upsert_overwrites_existing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

        db.upsert(&Note::new("n1", "original")).await?;
        db.upsert(&Note::new("n1", "updated")).await?;

        let loaded = db
            .get("n1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("note should exist after upsert"))?;
        assert_eq!(loaded.body, "updated");

        let all = db.list().await?;
        assert_eq!(all.len(), 1, "upsert should not duplicate rows");
        Ok(())
    }

    #[tokio::test]
    async fn list_returns_all_entities() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

        db.upsert(&Note::new("n1", "a")).await?;
        db.upsert(&Note::new("n2", "b")).await?;
        db.upsert(&Note::new("n3", "c")).await?;

        let all = db.list().await?;
        assert_eq!(all.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn delete_returns_true_when_found() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

        db.upsert(&Note::new("n1", "bye")).await?;
        assert!(db.delete("n1").await?);
        assert!(db.get("n1").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn delete_returns_false_when_missing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

        assert!(!db.delete("nonexistent").await?);
        Ok(())
    }

    #[tokio::test]
    async fn survives_reopen() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("notes.db");

        {
            let db = Db::<Note>::open(&db_path).await?;
            db.upsert(&Note::new("persistent", "content")).await?;
        }

        let db = Db::<Note>::open(&db_path).await?;
        let loaded = db
            .get("persistent")
            .await?
            .ok_or_else(|| anyhow::anyhow!("note should survive reopen"))?;
        assert_eq!(loaded.body, "content");
        Ok(())
    }
}
