use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::{pool::PoolConnection, Sqlite};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

// Postgres pool helpers and PgMigrator live in the sibling db_pg module.
// Re-export them here so existing callers using `harness_core::db::*` continue
// to work without any import changes.
pub use crate::db_pg::{pg_create_schema, pg_open_pool, pg_open_pool_schematized, PgMigrator};

static SQLITE_TRANSACTIONAL_PREFIXES: OnceLock<Vec<&'static str>> = OnceLock::new();

fn sqlite_transactional_prefixes() -> &'static [&'static str] {
    SQLITE_TRANSACTIONAL_PREFIXES.get_or_init(|| {
        vec![
            "CREATE TABLE",
            "CREATE INDEX",
            "CREATE UNIQUE INDEX",
            "CREATE VIEW",
            "CREATE TRIGGER",
            "ALTER TABLE",
            "DROP TABLE",
            "DROP INDEX",
            "DROP VIEW",
            "DROP TRIGGER",
            "INSERT INTO",
            "UPDATE ",
            "DELETE FROM",
            "REPLACE INTO",
            "SELECT ",
            "WITH ",
        ]
    })
}

fn normalized_sqlite_statement_prefix(statement: &str) -> String {
    statement
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("--"))
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

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

async fn apply_statements_on_connection(
    conn: &mut PoolConnection<Sqlite>,
    migration: &Migration,
    statements: &[MigrationStatement<'_>],
) -> anyhow::Result<()> {
    for statement in statements {
        if let Err(error) = sqlx::query(statement.sql.as_ref())
            .execute(conn.as_mut())
            .await
        {
            if duplicate_add_column_error(statement.sql.as_ref(), &error) {
                continue;
            }
            return Err(format_migration_error(migration, statement, &error, false));
        }
    }

    sqlx::query("INSERT INTO schema_migrations (version, description) VALUES (?, ?)")
        .bind(migration.version as i64)
        .bind(migration.description)
        .execute(conn.as_mut())
        .await?;
    Ok(())
}

async fn apply_outside_transaction(
    pool: &SqlitePool,
    migration: &Migration,
    statements: &[MigrationStatement<'_>],
) -> anyhow::Result<()> {
    let mut conn = pool.acquire().await?;
    let result = apply_statements_on_connection(&mut conn, migration, statements).await;
    if result.is_err() {
        // Close rather than return to pool: connection-scoped state set by a
        // partially-executed migration (e.g. PRAGMA changes) must not leak to
        // future pool borrowers.
        let _ = conn.close().await;
    }
    result
}

fn duplicate_add_column_error(statement: &str, error: &sqlx::Error) -> bool {
    statement.to_ascii_uppercase().contains("ADD COLUMN")
        && error
            .to_string()
            .to_ascii_lowercase()
            .contains("duplicate column name")
}

fn sqlite_statement_is_transaction_safe(statement: &str) -> bool {
    let normalized = normalized_sqlite_statement_prefix(statement);
    sqlite_transactional_prefixes()
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
}

fn format_migration_error(
    migration: &Migration,
    statement: &MigrationStatement<'_>,
    error: &sqlx::Error,
    in_transaction: bool,
) -> anyhow::Error {
    let mode = if in_transaction {
        ""
    } else {
        " outside transaction"
    };
    anyhow::anyhow!(
        "migration v{} '{}' failed at statement {}{}: {} [sql: {}]",
        migration.version,
        migration.description,
        statement.index,
        mode,
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

async fn apply_migration(pool: &SqlitePool, migration: &Migration) -> anyhow::Result<()> {
    let statements = migration_statements(migration.sql);
    let use_transaction = statements
        .iter()
        .all(|statement| sqlite_statement_is_transaction_safe(statement.sql.as_ref()));

    if use_transaction {
        let mut tx = pool.begin().await?;
        for statement in &statements {
            if let Err(error) = sqlx::query(statement.sql.as_ref()).execute(&mut *tx).await {
                if duplicate_add_column_error(statement.sql.as_ref(), &error) {
                    continue;
                }
                return Err(format_migration_error(migration, statement, &error, true));
            }
        }
        sqlx::query("INSERT INTO schema_migrations (version, description) VALUES (?, ?)")
            .bind(migration.version as i64)
            .bind(migration.description)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(());
    }

    apply_outside_transaction(pool, migration, &statements).await
}

fn applied_versions_set(rows: Vec<(i64,)>) -> HashSet<u32> {
    rows.into_iter().map(|(version,)| version as u32).collect()
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
        let applied = applied_versions_set(rows);

        for migration in pending_migrations(self.migrations, &applied) {
            apply_migration(self.pool, migration).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
