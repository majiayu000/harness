use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::Path;

/// Create a SQLite connection pool for the given path.
///
/// All stores share this configuration: 8 max connections, 10 s acquire
/// timeout, WAL journal mode, and 5 s busy timeout.
pub async fn open_pool(path: &Path) -> anyhow::Result<SqlitePool> {
    let url = format!("sqlite:{}?mode=rwc", path.display());
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&url)
        .await?;
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA busy_timeout=5000")
        .execute(&pool)
        .await?;
    Ok(pool)
}

/// Marker trait for entities that can be stored as JSON blobs.
pub trait DbEntity: Serialize + for<'de> Deserialize<'de> + Send + Unpin + 'static {
    /// SQL table name for this entity type.
    fn table_name() -> &'static str;
    /// Primary key value for this entity instance.
    fn id(&self) -> &str;
    /// DDL executed once on open to ensure the table exists.
    fn create_table_sql() -> &'static str;
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
/// keep a specialised store and call [`open_pool`] for pool creation.
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

    /// Expose the underlying pool for stores that need custom queries
    /// beyond the generic CRUD operations (e.g. `json_extract` filters).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// Trait for types that can be serialized to/from a SQLite status column.
///
/// Implementing this trait once per status type is sufficient — the blanket
/// `AsRef<str>` and `FromStr` impls are provided by callers that delegate to
/// these methods, eliminating duplicated match arms across DB modules.
pub trait DbSerializable: Sized {
    /// Return the canonical database string for this value.
    fn to_db_str(&self) -> &'static str;
    /// Parse a database string back into the value, or return an error.
    fn from_db_str(s: &str) -> anyhow::Result<Self>;
}

/// A single versioned SQL migration.
pub struct Migration {
    /// Monotonically increasing version number. Applied in ascending order.
    pub version: u32,
    /// Human-readable description stored in the `schema_migrations` table.
    pub description: &'static str,
    /// One or more semicolon-separated SQL statements to execute.
    pub sql: &'static str,
}

/// Runs versioned SQL migrations against a pool.
///
/// Maintains a `schema_migrations` table to track which versions have been
/// applied. Safe to call on every startup — already-applied versions are
/// skipped.
///
/// For `ALTER TABLE ADD COLUMN` statements on pre-existing databases,
/// "duplicate column name" errors are silently ignored so that migrating
/// databases that predate the migration system is idempotent.
pub struct Migrator<'a> {
    pool: &'a SqlitePool,
    migrations: &'a [Migration],
}

impl<'a> Migrator<'a> {
    pub fn new(pool: &'a SqlitePool, migrations: &'a [Migration]) -> Self {
        Self { pool, migrations }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        // Create the migrations tracking table if it doesn't exist yet.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version     INTEGER PRIMARY KEY,
                description TEXT NOT NULL,
                applied_at  TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(self.pool)
        .await?;

        // Fetch already-applied version numbers.
        let rows: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM schema_migrations ORDER BY version ASC")
                .fetch_all(self.pool)
                .await?;
        let applied: std::collections::HashSet<u32> =
            rows.into_iter().map(|(v,)| v as u32).collect();

        // Collect and sort pending migrations.
        let mut pending: Vec<&Migration> = self
            .migrations
            .iter()
            .filter(|m| !applied.contains(&m.version))
            .collect();
        pending.sort_by_key(|m| m.version);

        for migration in pending {
            for stmt in migration.sql.split(';') {
                let stmt = stmt.trim();
                if stmt.is_empty() {
                    continue;
                }
                if let Err(e) = sqlx::query(stmt).execute(self.pool).await {
                    // Tolerate duplicate-column errors from ALTER TABLE ADD COLUMN
                    // so that migrating pre-migration-system databases is idempotent.
                    if stmt.to_uppercase().contains("ADD COLUMN") {
                        let msg = e.to_string().to_lowercase();
                        if msg.contains("duplicate column name") {
                            continue;
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "migration v{} '{}' failed: {e}",
                        migration.version,
                        migration.description
                    ));
                }
            }
            sqlx::query("INSERT INTO schema_migrations (version, description) VALUES (?, ?)")
                .bind(migration.version as i64)
                .bind(migration.description)
                .execute(self.pool)
                .await?;
        }
        Ok(())
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

    // --- Migrator tests ---

    static SIMPLE_MIGRATIONS: &[Migration] = &[
        Migration {
            version: 1,
            description: "create items table",
            sql: "CREATE TABLE IF NOT EXISTS items (id TEXT PRIMARY KEY, value TEXT NOT NULL)",
        },
        Migration {
            version: 2,
            description: "add tag column",
            sql: "ALTER TABLE items ADD COLUMN tag TEXT",
        },
    ];

    #[tokio::test]
    async fn migrator_applies_pending_migrations() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = open_pool(&dir.path().join("mig.db")).await?;

        Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;

        // Both versions should be recorded.
        let rows: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM schema_migrations ORDER BY version")
                .fetch_all(&pool)
                .await?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 1);
        assert_eq!(rows[1].0, 2);
        Ok(())
    }

    #[tokio::test]
    async fn migrator_is_idempotent_on_rerun() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = open_pool(&dir.path().join("mig.db")).await?;

        Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;
        // Running again must not fail or duplicate rows.
        Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;

        let rows: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM schema_migrations ORDER BY version")
                .fetch_all(&pool)
                .await?;
        assert_eq!(rows.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn migrator_tolerates_duplicate_column_on_alter_table() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = open_pool(&dir.path().join("mig.db")).await?;

        // Run only version 1 first (creates the table).
        Migrator::new(&pool, &SIMPLE_MIGRATIONS[..1]).run().await?;

        // Manually add the column that migration v2 would add.
        sqlx::query("ALTER TABLE items ADD COLUMN tag TEXT")
            .execute(&pool)
            .await?;

        // Running v2 now must not fail even though the column already exists.
        Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;
        Ok(())
    }
}
