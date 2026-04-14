use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Normalize path components without filesystem access.
///
/// Resolves `.` (current-dir) and `..` (parent-dir) components so that
/// logically equivalent paths (`/a/./b`, `/a/c/../b`) produce the same
/// string before hashing.
fn normalize_path_components(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// FNV-1a 64-bit hash — deterministic, no randomization, no extra dependencies.
fn fnv1a_64(data: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut hash = OFFSET_BASIS;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Derive a deterministic, collision-resistant, Postgres-safe schema name from a path.
///
/// Each unique path (e.g. per-store data directory or per-test tempdir) gets
/// its own schema, providing namespace isolation between stores and between
/// test runs without requiring separate databases.
///
/// The algorithm: normalize `.`/`..` components (no I/O), hash with FNV-1a
/// 64-bit, format as `hs_<16-hex-digits>`.  Distinct paths map to distinct
/// schemas with overwhelming probability (2⁻⁶⁴ per pair); logically
/// equivalent paths always map to the same schema.
fn path_to_schema(path: &Path) -> String {
    let normalized = normalize_path_components(path);
    let s = normalized.to_string_lossy();
    let hash = fnv1a_64(s.as_bytes());
    format!("hs_{hash:016x}")
}

/// Create a PostgreSQL connection pool scoped to a per-path schema.
///
/// The connection URL is read from the `DATABASE_URL` environment variable.
/// Each unique `path` value maps to a dedicated Postgres schema (derived via
/// [`path_to_schema`]), which is created automatically on first connection and
/// set as the default `search_path` for every connection in the pool.
///
/// This provides store-level isolation: tables from `task_db`, `thread_db`,
/// `event_store`, etc. each live in their own schema and never collide — even
/// when all stores share a single Postgres database.
///
/// All stores share this configuration: 8 max connections, 10 s acquire timeout.
pub async fn open_pool(path: &Path) -> anyhow::Result<PgPool> {
    let url = std::env::var("DATABASE_URL")
        .map_err(|_| anyhow::anyhow!("DATABASE_URL environment variable must be set"))?;
    let schema = path_to_schema(path);
    PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .after_connect(move |conn, _meta| {
            let schema = schema.clone();
            Box::pin(async move {
                sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
                    .execute(&mut *conn)
                    .await?;
                sqlx::query(&format!("SET search_path TO \"{schema}\""))
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .map_err(Into::into)
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

/// Generic PostgreSQL store that persists entities as JSON blobs.
///
/// Schema:
/// ```sql
/// CREATE TABLE IF NOT EXISTS <table_name> (
///     id         TEXT PRIMARY KEY,
///     data       TEXT NOT NULL,
///     created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
///     updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
/// )
/// ```
///
/// Use this when entities do not need queryable columns beyond `id`.
/// For entities requiring SQL-level filtering (e.g. `WHERE status = $1`),
/// keep a specialised store and call [`open_pool`] for pool creation.
pub struct Db<T: DbEntity> {
    pub(crate) pool: PgPool,
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
            "INSERT INTO {table} (id, data) VALUES ($1, $2)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data,
                 updated_at = CURRENT_TIMESTAMP",
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
        let sql = format!("SELECT data FROM {} WHERE id = $1", T::table_name());
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
        let sql = format!("DELETE FROM {} WHERE id = $1", T::table_name());
        let result = sqlx::query(&sql).bind(id).execute(&self.pool).await?;
        Ok(result.rows_affected() > 0)
    }

    /// Expose the underlying pool for stores that need custom queries
    /// beyond the generic CRUD operations (e.g. JSONB field filters).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Trait for types that can be serialized to/from a database status column.
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
/// Maintains a per-store tracking table (default: `schema_migrations`) to
/// record which versions have been applied. Using a per-store table avoids
/// version-number collisions when multiple stores share a single Postgres
/// database — each store's version sequence is independent.
///
/// Safe to call on every startup — already-applied versions are skipped.
///
/// For `ALTER TABLE ADD COLUMN` statements on pre-existing databases,
/// "column already exists" errors are silently ignored so that migrating
/// databases that predate the migration system is idempotent.
pub struct Migrator<'a> {
    pool: &'a PgPool,
    migrations: &'a [Migration],
    tracking_table: &'static str,
}

struct MigrationStatement<'a> {
    index: usize,
    sql: Cow<'a, str>,
}

fn migration_statements(sql: &str) -> Vec<MigrationStatement<'_>> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();
    let mut index = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut trigger_begin_depth = 0usize;
    let mut trigger_case_depth = 0usize;
    let mut statement_tokens = Vec::new();
    let mut current_statement_is_trigger = false;

    while let Some(ch) = chars.next() {
        let next = chars.peek().copied();

        if in_line_comment {
            current.push(ch);
            if ch == '\n' {
                in_line_comment = false;
            }
            continue;
        }

        if in_block_comment {
            current.push(ch);
            if ch == '*' && next == Some('/') {
                current.push(chars.next().expect("peeked slash must exist"));
                in_block_comment = false;
            }
            continue;
        }

        if in_single_quote {
            current.push(ch);
            if ch == '\'' {
                if next == Some('\'') {
                    current.push(chars.next().expect("peeked quote must exist"));
                } else {
                    in_single_quote = false;
                }
            }
            continue;
        }

        if in_double_quote {
            current.push(ch);
            if ch == '"' {
                if next == Some('"') {
                    current.push(chars.next().expect("peeked quote must exist"));
                } else {
                    in_double_quote = false;
                }
            }
            continue;
        }

        if ch == '-' && next == Some('-') {
            current.push(ch);
            current.push(chars.next().expect("peeked dash must exist"));
            in_line_comment = true;
            continue;
        }

        if ch == '/' && next == Some('*') {
            current.push(ch);
            current.push(chars.next().expect("peeked star must exist"));
            in_block_comment = true;
            continue;
        }

        if ch == '\'' {
            current.push(ch);
            in_single_quote = true;
            continue;
        }

        if ch == '"' {
            current.push(ch);
            in_double_quote = true;
            continue;
        }

        current.push(ch);

        if ch.is_ascii_alphabetic() || ch == '_' {
            let mut token = String::from(ch);
            while let Some(peek) = chars.peek().copied() {
                if peek.is_ascii_alphanumeric() || peek == '_' {
                    token.push(chars.next().expect("peeked token char must exist"));
                    current.push(*token.as_bytes().last().unwrap() as char);
                } else {
                    break;
                }
            }

            let token = token.to_ascii_uppercase();
            if statement_tokens.len() < 3 {
                statement_tokens.push(token.clone());
                current_statement_is_trigger |= match statement_tokens.as_slice() {
                    [first, second] => first == "CREATE" && second == "TRIGGER",
                    [first, second, third] => {
                        first == "CREATE"
                            && (second == "TEMP" || second == "TEMPORARY")
                            && third == "TRIGGER"
                    }
                    _ => false,
                };
            }

            match token.as_str() {
                "BEGIN" if current_statement_is_trigger => trigger_begin_depth += 1,
                "CASE" if current_statement_is_trigger && trigger_begin_depth > 0 => {
                    trigger_case_depth += 1;
                }
                "END"
                    if current_statement_is_trigger
                        && trigger_begin_depth > 0
                        && trigger_case_depth > 0 =>
                {
                    trigger_case_depth -= 1;
                }
                "END" if current_statement_is_trigger && trigger_begin_depth > 0 => {
                    trigger_begin_depth -= 1;
                }
                _ => {}
            }
            if trigger_begin_depth == 0 {
                trigger_case_depth = 0;
            }
            continue;
        }

        if ch == ';' && trigger_begin_depth == 0 {
            let statement = current[..current.len() - 1].trim();
            if !statement.is_empty() {
                index += 1;
                statements.push(MigrationStatement {
                    index,
                    sql: Cow::Owned(statement.to_string()),
                });
            }
            current.clear();
            statement_tokens.clear();
            current_statement_is_trigger = false;
        }
    }

    let trailing = current.trim();
    if !trailing.is_empty() {
        index += 1;
        statements.push(MigrationStatement {
            index,
            sql: Cow::Owned(trailing.to_string()),
        });
    }

    statements
}

fn duplicate_add_column_error(statement: &str, error: &sqlx::Error) -> bool {
    statement.to_ascii_uppercase().contains("ADD COLUMN")
        && error
            .to_string()
            .to_ascii_lowercase()
            .contains("already exists")
}

fn format_migration_error(
    migration: &Migration,
    statement: &MigrationStatement<'_>,
    error: &sqlx::Error,
) -> anyhow::Error {
    anyhow::anyhow!(
        "migration v{} '{}' failed at statement {}: {} [sql: {}]",
        migration.version,
        migration.description,
        statement.index,
        error,
        statement.sql
    )
}

fn pending_migrations<'a>(
    migrations: &'a [Migration],
    applied: &HashSet<u32>,
) -> Vec<&'a Migration> {
    let mut pending: Vec<&Migration> = migrations
        .iter()
        .filter(|migration| !applied.contains(&migration.version))
        .collect();
    pending.sort_by_key(|migration| migration.version);
    pending
}

/// Apply a migration inside a single transaction. PostgreSQL supports
/// transactional DDL, so all statements always run in a transaction.
///
/// For `ALTER TABLE ADD COLUMN` statements that fail with "already exists",
/// a savepoint is used to roll back only that statement so the outer
/// transaction stays healthy and subsequent statements can proceed.
async fn apply_migration(
    pool: &PgPool,
    migration: &Migration,
    tracking_table: &str,
) -> anyhow::Result<()> {
    let statements = migration_statements(migration.sql);
    let mut tx = pool.begin().await?;
    for statement in &statements {
        sqlx::query("SAVEPOINT migration_sp")
            .execute(&mut *tx)
            .await?;
        if let Err(error) = sqlx::query(statement.sql.as_ref()).execute(&mut *tx).await {
            if duplicate_add_column_error(statement.sql.as_ref(), &error) {
                sqlx::query("ROLLBACK TO SAVEPOINT migration_sp")
                    .execute(&mut *tx)
                    .await?;
                continue;
            }
            return Err(format_migration_error(migration, statement, &error));
        }
        sqlx::query("RELEASE SAVEPOINT migration_sp")
            .execute(&mut *tx)
            .await?;
    }
    let insert_sql = format!("INSERT INTO {tracking_table} (version, description) VALUES ($1, $2)");
    sqlx::query(&insert_sql)
        .bind(migration.version as i64)
        .bind(migration.description)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

fn applied_versions_set(rows: Vec<(i64,)>) -> HashSet<u32> {
    rows.into_iter().map(|(version,)| version as u32).collect()
}

impl<'a> Migrator<'a> {
    pub fn new(pool: &'a PgPool, migrations: &'a [Migration]) -> Self {
        Self {
            pool,
            migrations,
            tracking_table: "schema_migrations",
        }
    }

    /// Override the tracking table name (default: `"schema_migrations"`).
    ///
    /// Use this when multiple stores share a single Postgres database so that
    /// each store's migration versions are tracked independently, preventing
    /// version-number collisions between stores.
    pub fn with_tracking_table(mut self, table: &'static str) -> Self {
        self.tracking_table = table;
        self
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        // Create the per-store migrations tracking table if it doesn't exist.
        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (
                version     BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                applied_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
            self.tracking_table
        );
        sqlx::query(&create_sql).execute(self.pool).await?;

        // Fetch already-applied version numbers.
        let select_sql = format!(
            "SELECT version FROM {} ORDER BY version ASC",
            self.tracking_table
        );
        let rows: Vec<(i64,)> = sqlx::query_as(&select_sql).fetch_all(self.pool).await?;
        let applied = applied_versions_set(rows);

        for migration in pending_migrations(self.migrations, &applied) {
            apply_migration(self.pool, migration, self.tracking_table).await?;
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
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )"
        }
    }

    async fn open_test_pool() -> anyhow::Result<PgPool> {
        let url = std::env::var("DATABASE_URL")
            .map_err(|_| anyhow::anyhow!("DATABASE_URL must be set to run DB tests"))?;
        PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .map_err(Into::into)
    }

    #[tokio::test]
    async fn upsert_and_get_roundtrip() -> anyhow::Result<()> {
        let pool = open_test_pool().await?;
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&pool)
            .await?;
        let db = Db::<Note> {
            pool,
            _phantom: std::marker::PhantomData,
        };
        db.migrate().await?;

        let note = Note::new("n1", "hello");
        db.upsert(&note).await?;

        let loaded = db
            .get("n1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("note should exist"))?;
        assert_eq!(loaded, note);
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&db.pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn get_returns_none_for_missing() -> anyhow::Result<()> {
        let pool = open_test_pool().await?;
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&pool)
            .await?;
        let db = Db::<Note> {
            pool,
            _phantom: std::marker::PhantomData,
        };
        db.migrate().await?;

        assert!(db.get("missing").await?.is_none());
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&db.pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn upsert_overwrites_existing() -> anyhow::Result<()> {
        let pool = open_test_pool().await?;
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&pool)
            .await?;
        let db = Db::<Note> {
            pool,
            _phantom: std::marker::PhantomData,
        };
        db.migrate().await?;

        db.upsert(&Note::new("n1", "original")).await?;
        db.upsert(&Note::new("n1", "updated")).await?;

        let loaded = db
            .get("n1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("note should exist after upsert"))?;
        assert_eq!(loaded.body, "updated");

        let all = db.list().await?;
        assert_eq!(all.len(), 1, "upsert should not duplicate rows");
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&db.pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn list_returns_all_entities() -> anyhow::Result<()> {
        let pool = open_test_pool().await?;
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&pool)
            .await?;
        let db = Db::<Note> {
            pool,
            _phantom: std::marker::PhantomData,
        };
        db.migrate().await?;

        db.upsert(&Note::new("n1", "a")).await?;
        db.upsert(&Note::new("n2", "b")).await?;
        db.upsert(&Note::new("n3", "c")).await?;

        let all = db.list().await?;
        assert_eq!(all.len(), 3);
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&db.pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn delete_returns_true_when_found() -> anyhow::Result<()> {
        let pool = open_test_pool().await?;
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&pool)
            .await?;
        let db = Db::<Note> {
            pool,
            _phantom: std::marker::PhantomData,
        };
        db.migrate().await?;

        db.upsert(&Note::new("n1", "bye")).await?;
        assert!(db.delete("n1").await?);
        assert!(db.get("n1").await?.is_none());
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&db.pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn delete_returns_false_when_missing() -> anyhow::Result<()> {
        let pool = open_test_pool().await?;
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&pool)
            .await?;
        let db = Db::<Note> {
            pool,
            _phantom: std::marker::PhantomData,
        };
        db.migrate().await?;

        assert!(!db.delete("nonexistent").await?);
        sqlx::query("DROP TABLE IF EXISTS notes")
            .execute(&db.pool)
            .await?;
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

    async fn applied_versions(pool: &PgPool) -> anyhow::Result<Vec<i64>> {
        Ok(
            sqlx::query_as::<_, (i64,)>("SELECT version FROM schema_migrations ORDER BY version")
                .fetch_all(pool)
                .await?
                .into_iter()
                .map(|(version,)| version)
                .collect(),
        )
    }

    #[tokio::test]
    async fn migrator_applies_pending_migrations() -> anyhow::Result<()> {
        let pool = open_test_pool().await?;
        sqlx::query("DROP TABLE IF EXISTS items")
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM schema_migrations WHERE version IN (1, 2)")
            .execute(&pool)
            .await
            .ok();

        Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;

        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT version FROM schema_migrations WHERE version IN (1,2) ORDER BY version",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 1);
        assert_eq!(rows[1].0, 2);
        sqlx::query("DROP TABLE IF EXISTS items")
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn migrator_is_idempotent_on_rerun() -> anyhow::Result<()> {
        let pool = open_test_pool().await?;
        sqlx::query("DROP TABLE IF EXISTS items")
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM schema_migrations WHERE version IN (1, 2)")
            .execute(&pool)
            .await
            .ok();

        Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;
        Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;

        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT version FROM schema_migrations WHERE version IN (1,2) ORDER BY version",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(rows.len(), 2);
        sqlx::query("DROP TABLE IF EXISTS items")
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn migrator_tolerates_duplicate_column_on_alter_table() -> anyhow::Result<()> {
        let pool = open_test_pool().await?;
        sqlx::query("DROP TABLE IF EXISTS items")
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM schema_migrations WHERE version IN (1, 2)")
            .execute(&pool)
            .await
            .ok();

        Migrator::new(&pool, &SIMPLE_MIGRATIONS[..1]).run().await?;

        // Manually add the column that migration v2 would add.
        sqlx::query("ALTER TABLE items ADD COLUMN tag TEXT")
            .execute(&pool)
            .await?;

        // Running v2 now must not fail even though the column already exists.
        Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;
        let versions = applied_versions(&pool).await?;
        assert!(versions.contains(&1));
        assert!(versions.contains(&2));
        sqlx::query("DROP TABLE IF EXISTS items")
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn failing_multi_statement_migration_is_not_recorded() -> anyhow::Result<()> {
        let pool = open_test_pool().await?;
        sqlx::query("DROP TABLE IF EXISTS mig_test_items")
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM schema_migrations WHERE version IN (100, 101)")
            .execute(&pool)
            .await
            .ok();

        let migrations = [
            Migration {
                version: 100,
                description: "create mig_test_items table",
                sql: "CREATE TABLE mig_test_items (id TEXT PRIMARY KEY)",
            },
            Migration {
                version: 101,
                description: "broken migration",
                sql: "ALTER TABLE mig_test_items ADD COLUMN tag TEXT; SELECT nope FROM missing_table_xyz",
            },
        ];

        let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();

        assert!(
            err.to_string()
                .contains("migration v101 'broken migration' failed"),
            "unexpected error: {err:#}"
        );
        let versions = applied_versions(&pool).await?;
        assert!(versions.contains(&100));
        assert!(!versions.contains(&101));
        sqlx::query("DROP TABLE IF EXISTS mig_test_items")
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn migration_error_reports_failed_statement_index() -> anyhow::Result<()> {
        let pool = open_test_pool().await?;
        sqlx::query("DROP TABLE IF EXISTS mig_idx_items")
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM schema_migrations WHERE version IN (200, 201)")
            .execute(&pool)
            .await
            .ok();

        let migrations = [
            Migration {
                version: 200,
                description: "create mig_idx_items table",
                sql: "CREATE TABLE mig_idx_items (id TEXT PRIMARY KEY)",
            },
            Migration {
                version: 201,
                description: "broken migration",
                sql: "ALTER TABLE mig_idx_items ADD COLUMN tag TEXT; SELECT nope FROM missing_table_xyz",
            },
        ];

        let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();
        let message = err.to_string();

        assert!(
            message.contains("migration v201 'broken migration' failed at statement 2"),
            "unexpected error: {message}"
        );
        sqlx::query("DROP TABLE IF EXISTS mig_idx_items")
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[test]
    fn migration_statements_keep_trigger_body_intact() {
        let statements = migration_statements(
            "CREATE TABLE items (id INTEGER PRIMARY KEY);\n\
             CREATE TRIGGER items_touch AFTER INSERT ON items BEGIN\n\
             UPDATE items SET id = NEW.id;\n\
             INSERT INTO items_log DEFAULT VALUES;\n\
             END;",
        );

        assert_eq!(statements.len(), 2);
        assert_eq!(
            statements[0].sql,
            "CREATE TABLE items (id INTEGER PRIMARY KEY)"
        );
        assert!(
            statements[1]
                .sql
                .starts_with("CREATE TRIGGER items_touch AFTER INSERT ON items BEGIN"),
            "unexpected trigger statement: {}",
            statements[1].sql
        );
        assert!(
            statements[1].sql.ends_with("END"),
            "unexpected trigger statement: {}",
            statements[1].sql
        );
    }

    #[test]
    fn migration_statements_split_explicit_transaction_control() {
        let statements = migration_statements(
            "BEGIN;\n\
             CREATE TABLE items (id INTEGER PRIMARY KEY);\n\
             INSERT INTO items VALUES (1);\n\
             COMMIT;",
        );

        assert_eq!(statements.len(), 4);
        assert_eq!(statements[0].sql, "BEGIN");
        assert_eq!(
            statements[1].sql,
            "CREATE TABLE items (id INTEGER PRIMARY KEY)"
        );
        assert_eq!(statements[2].sql, "INSERT INTO items VALUES (1)");
        assert_eq!(statements[3].sql, "COMMIT");
    }

    #[test]
    fn migration_statements_keep_case_end_inside_trigger_body() {
        let statements = migration_statements(
            "CREATE TRIGGER items_touch AFTER INSERT ON items BEGIN\n\
             UPDATE items\n\
             SET id = CASE WHEN NEW.id IS NULL THEN OLD.id ELSE NEW.id END;\n\
             INSERT INTO items_log DEFAULT VALUES;\n\
             END;",
        );

        assert_eq!(statements.len(), 1);
        assert!(
            statements[0]
                .sql
                .contains("CASE WHEN NEW.id IS NULL THEN OLD.id ELSE NEW.id END;"),
            "unexpected trigger statement: {}",
            statements[0].sql
        );
        assert!(
            statements[0]
                .sql
                .contains("INSERT INTO items_log DEFAULT VALUES;"),
            "unexpected trigger statement: {}",
            statements[0].sql
        );
        assert!(
            statements[0].sql.ends_with("END"),
            "unexpected trigger statement: {}",
            statements[0].sql
        );
    }
}
