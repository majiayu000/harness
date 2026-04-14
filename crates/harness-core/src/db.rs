use serde::{Deserialize, Serialize};
use sqlx::any::AnyPoolOptions;
use sqlx::pool::PoolConnection;
use sqlx::{Any, AnyPool};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

static DRIVERS_INSTALLED: OnceLock<()> = OnceLock::new();

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

/// Supported database dialects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
}

impl Dialect {
    /// Detect dialect from a connection URL prefix.
    ///
    /// Accepts both `postgres://` and `postgresql://` (both are valid DSN schemes
    /// recognised by sqlx; checking only `"postgres"` mis-classifies `postgresql://`
    /// as SQLite and causes startup failures when PRAGMAs are executed against Postgres).
    pub fn from_url(url: &str) -> Self {
        if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            Self::Postgres
        } else {
            Self::Sqlite
        }
    }
}

/// Rewrite `?` positional placeholders to `$1`, `$2`, … for Postgres.
///
/// SQLite SQL is returned unchanged. Single-quoted string literals are skipped
/// so that a literal `?` inside a string value is not replaced.
pub fn rewrite_placeholders(sql: &str, dialect: Dialect) -> String {
    if dialect == Dialect::Sqlite {
        return sql.to_string();
    }
    let mut result = String::with_capacity(sql.len() + 16);
    let mut counter = 0usize;
    let mut chars = sql.chars().peekable();
    let mut in_single_quote = false;
    while let Some(ch) = chars.next() {
        if in_single_quote {
            result.push(ch);
            if ch == '\'' {
                match chars.peek() {
                    Some('\'') => {
                        // Escaped single quote inside string literal.
                        result.push(chars.next().unwrap_or_default());
                    }
                    _ => in_single_quote = false,
                }
            }
        } else if ch == '\'' {
            result.push(ch);
            in_single_quote = true;
        } else if ch == '?' {
            counter += 1;
            result.push('$');
            result.push_str(&counter.to_string());
        } else {
            result.push(ch);
        }
    }
    result
}

/// Create a connection pool for the given SQLite file path.
///
/// All stores share this configuration: 8 max connections, 10 s acquire
/// timeout, WAL journal mode, and 5 s busy timeout.
///
/// To connect to Postgres or use an explicit URL, call [`open_pool_url`].
pub async fn open_pool(path: &Path) -> anyhow::Result<AnyPool> {
    let url = format!("sqlite:{}?mode=rwc", path.display());
    open_pool_url(&url).await
}

/// Create a connection pool from an explicit DSN (`sqlite:…` or `postgres://…`).
///
/// SQLite connections receive WAL mode and busy_timeout PRAGMAs automatically.
pub async fn open_pool_url(url: &str) -> anyhow::Result<AnyPool> {
    DRIVERS_INSTALLED.get_or_init(sqlx::any::install_default_drivers);
    let dialect = Dialect::from_url(url);
    let pool = AnyPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(url)
        .await?;
    if dialect == Dialect::Sqlite {
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await?;
        sqlx::query("PRAGMA busy_timeout=5000")
            .execute(&pool)
            .await?;
    }
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

/// Generic store that persists entities as JSON blobs.
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
/// For entities requiring SQL-level filtering (e.g. `WHERE status = ?`),
/// keep a specialised store and call [`open_pool`] for pool creation.
pub struct Db<T: DbEntity> {
    pub(crate) pool: AnyPool,
    dialect: Dialect,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: DbEntity> Db<T> {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let pool = open_pool(path).await?;
        let db = Self {
            pool,
            dialect: Dialect::Sqlite,
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
        let raw = format!(
            "INSERT INTO {table} (id, data) VALUES (?, ?)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data,
                 updated_at = CURRENT_TIMESTAMP",
            table = T::table_name()
        );
        let sql = rewrite_placeholders(&raw, self.dialect);
        sqlx::query(&sql)
            .bind(entity.id())
            .bind(&data)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<T>> {
        let raw = format!("SELECT data FROM {} WHERE id = ?", T::table_name());
        let sql = rewrite_placeholders(&raw, self.dialect);
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
        let raw = format!("DELETE FROM {} WHERE id = ?", T::table_name());
        let sql = rewrite_placeholders(&raw, self.dialect);
        let result = sqlx::query(&sql).bind(id).execute(&self.pool).await?;
        Ok(result.rows_affected() > 0)
    }

    /// Expose the underlying pool for stores that need custom queries
    /// beyond the generic CRUD operations (e.g. `json_extract` filters).
    pub fn pool(&self) -> &AnyPool {
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
/// Maintains a `schema_migrations` table to track which versions have been
/// applied. Safe to call on every startup — already-applied versions are
/// skipped.
///
/// For `ALTER TABLE ADD COLUMN` statements on pre-existing databases,
/// "duplicate column name" errors are silently ignored so that migrating
/// databases that predate the migration system is idempotent. For Postgres,
/// the equivalent "already exists" error is also silently ignored.
pub struct Migrator<'a> {
    pool: &'a AnyPool,
    migrations: &'a [Migration],
    dialect: Dialect,
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
    conn: &mut PoolConnection<Any>,
    migration: &Migration,
    statements: &[MigrationStatement<'_>],
    dialect: Dialect,
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

    let insert_sql = rewrite_placeholders(
        "INSERT INTO schema_migrations (version, description) VALUES (?, ?)",
        dialect,
    );
    sqlx::query(&insert_sql)
        .bind(migration.version as i64)
        .bind(migration.description)
        .execute(conn.as_mut())
        .await?;
    Ok(())
}

async fn apply_outside_transaction(
    pool: &AnyPool,
    migration: &Migration,
    statements: &[MigrationStatement<'_>],
    dialect: Dialect,
) -> anyhow::Result<()> {
    let mut conn = pool.acquire().await?;
    let result = apply_statements_on_connection(&mut conn, migration, statements, dialect).await;
    if result.is_err() {
        // Close rather than return to pool: connection-scoped state set by a
        // partially-executed migration (e.g. PRAGMA changes) must not leak to
        // future pool borrowers.
        let _ = conn.close().await;
    }
    result
}

fn duplicate_add_column_error(statement: &str, error: &sqlx::Error) -> bool {
    if !statement.to_ascii_uppercase().contains("ADD COLUMN") {
        return false;
    }
    let msg = error.to_string().to_ascii_lowercase();
    // SQLite: "duplicate column name"
    // Postgres: "column ... of relation ... already exists"
    msg.contains("duplicate column name") || msg.contains("already exists")
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

async fn apply_migration(
    pool: &AnyPool,
    migration: &Migration,
    dialect: Dialect,
) -> anyhow::Result<()> {
    let statements = migration_statements(migration.sql);
    let use_transaction = statements
        .iter()
        .all(|statement| sqlite_statement_is_transaction_safe(statement.sql.as_ref()));

    if use_transaction {
        let mut tx = pool.begin().await?;
        for statement in &statements {
            // In Postgres any failed statement marks the entire transaction aborted,
            // so we cannot simply `continue` after a duplicate-column error — the
            // next statement (or the schema_migrations insert) would fail with
            // "current transaction is aborted".  Use a savepoint to isolate the
            // error and roll back only that statement, leaving the transaction valid.
            let exec_result = if dialect == Dialect::Postgres {
                sqlx::query("SAVEPOINT _mig").execute(&mut *tx).await?;
                let res = sqlx::query(statement.sql.as_ref()).execute(&mut *tx).await;
                if res.is_ok() {
                    sqlx::query("RELEASE SAVEPOINT _mig")
                        .execute(&mut *tx)
                        .await?;
                } else {
                    sqlx::query("ROLLBACK TO SAVEPOINT _mig")
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("RELEASE SAVEPOINT _mig")
                        .execute(&mut *tx)
                        .await?;
                }
                res
            } else {
                sqlx::query(statement.sql.as_ref()).execute(&mut *tx).await
            };
            if let Err(error) = exec_result {
                if duplicate_add_column_error(statement.sql.as_ref(), &error) {
                    continue;
                }
                return Err(format_migration_error(migration, statement, &error, true));
            }
        }
        let insert_sql = rewrite_placeholders(
            "INSERT INTO schema_migrations (version, description) VALUES (?, ?)",
            dialect,
        );
        sqlx::query(&insert_sql)
            .bind(migration.version as i64)
            .bind(migration.description)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(());
    }

    apply_outside_transaction(pool, migration, &statements, dialect).await
}

fn applied_versions_set(rows: Vec<(i64,)>) -> HashSet<u32> {
    rows.into_iter().map(|(version,)| version as u32).collect()
}

impl<'a> Migrator<'a> {
    pub fn new(pool: &'a AnyPool, migrations: &'a [Migration]) -> Self {
        Self {
            pool,
            migrations,
            dialect: Dialect::Sqlite,
        }
    }

    /// Override the SQL dialect used when recording applied versions in
    /// `schema_migrations`.  Required when the pool was created via
    /// [`open_pool_url`] with a non-SQLite DSN (e.g. `postgres://…`); the
    /// default dialect is `Sqlite` for backwards compatibility.
    ///
    /// ```no_run
    /// # async fn example(pool: &sqlx::AnyPool) -> anyhow::Result<()> {
    /// use harness_core::db::{Dialect, Migration, Migrator};
    /// Migrator::new(pool, &[])
    ///     .with_dialect(Dialect::Postgres)
    ///     .run()
    ///     .await
    /// # }
    /// ```
    pub fn with_dialect(self, dialect: Dialect) -> Self {
        Self { dialect, ..self }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        // Create the migrations tracking table if it doesn't exist yet.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version     INTEGER PRIMARY KEY,
                description TEXT NOT NULL,
                applied_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
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
            apply_migration(self.pool, migration, self.dialect).await?;
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

    async fn applied_versions(pool: &AnyPool) -> anyhow::Result<Vec<i64>> {
        Ok(
            sqlx::query_as::<_, (i64,)>("SELECT version FROM schema_migrations ORDER BY version")
                .fetch_all(pool)
                .await?
                .into_iter()
                .map(|(version,)| version)
                .collect(),
        )
    }

    async fn table_columns(pool: &AnyPool, table: &str) -> anyhow::Result<Vec<String>> {
        let pragma = format!("PRAGMA table_info({table})");
        Ok(
            sqlx::query_as::<_, (i64, String, String, i64, Option<String>, i64)>(&pragma)
                .fetch_all(pool)
                .await?
                .into_iter()
                .map(|(_, name, _, _, _, _)| name)
                .collect(),
        )
    }

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
        assert_eq!(applied_versions(&pool).await?, vec![1, 2]);
        Ok(())
    }

    #[tokio::test]
    async fn failing_multi_statement_migration_is_not_recorded() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = open_pool(&dir.path().join("mig.db")).await?;
        let migrations = [
            Migration {
                version: 1,
                description: "create items table",
                sql: "CREATE TABLE items (id TEXT PRIMARY KEY)",
            },
            Migration {
                version: 2,
                description: "broken migration",
                sql: "ALTER TABLE items ADD COLUMN tag TEXT; SELECT nope FROM missing_table",
            },
        ];

        let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();

        assert!(
            err.to_string()
                .contains("migration v2 'broken migration' failed"),
            "unexpected error: {err:#}"
        );
        assert_eq!(applied_versions(&pool).await?, vec![1]);
        Ok(())
    }

    #[tokio::test]
    async fn failing_multi_statement_migration_does_not_partially_persist() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = open_pool(&dir.path().join("mig.db")).await?;
        let migrations = [
            Migration {
                version: 1,
                description: "create items table",
                sql: "CREATE TABLE items (id TEXT PRIMARY KEY)",
            },
            Migration {
                version: 2,
                description: "broken migration",
                sql: "ALTER TABLE items ADD COLUMN tag TEXT; SELECT nope FROM missing_table",
            },
        ];

        let _ = Migrator::new(&pool, &migrations).run().await;

        assert_eq!(applied_versions(&pool).await?, vec![1]);
        assert_eq!(table_columns(&pool, "items").await?, vec!["id"]);
        Ok(())
    }

    #[tokio::test]
    async fn failing_migration_with_syntax_error_reports_version() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = open_pool(&dir.path().join("mig.db")).await?;
        let migrations = [
            Migration {
                version: 1,
                description: "create items table",
                sql: "CREATE TABLE items (id TEXT PRIMARY KEY)",
            },
            Migration {
                version: 2,
                description: "broken migration",
                sql: "ALTER TABLE items ADD COLUMN tag TEXT; SELECT nope FROM missing_table",
            },
        ];

        let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();
        let message = err.to_string();

        assert!(
            message.contains("migration v2 'broken migration' failed at statement 2"),
            "unexpected error: {message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn pragma_foreign_keys_migration_runs_outside_transaction() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = open_pool(&dir.path().join("mig.db")).await?;
        let migrations = [
            Migration {
                version: 1,
                description: "create items table",
                sql: "CREATE TABLE items (id TEXT PRIMARY KEY)",
            },
            Migration {
                version: 2,
                description: "pragma migration",
                sql: "PRAGMA foreign_keys = OFF; SELECT nope FROM missing_table",
            },
        ];

        let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();
        let message = err.to_string();

        assert!(
            message.contains("outside transaction"),
            "unexpected error: {message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn multiline_create_table_migration_stays_transactional() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = open_pool(&dir.path().join("mig.db")).await?;
        let migrations = [Migration {
            version: 1,
            description: "broken multiline create table",
            sql: "CREATE\nTABLE items (id TEXT PRIMARY KEY); SELECT nope FROM missing_table",
        }];

        let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();
        let message = err.to_string();

        assert!(
            !message.contains("outside transaction"),
            "unexpected error: {message}"
        );
        assert_eq!(applied_versions(&pool).await?, Vec::<i64>::new());
        assert_eq!(table_columns(&pool, "items").await?, Vec::<String>::new());
        Ok(())
    }

    #[tokio::test]
    async fn pragma_defer_foreign_keys_migration_runs_outside_transaction() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = open_pool(&dir.path().join("mig.db")).await?;
        let migrations = [
            Migration {
                version: 1,
                description: "create items table",
                sql: "CREATE TABLE items (id TEXT PRIMARY KEY)",
            },
            Migration {
                version: 2,
                description: "pragma defer migration",
                sql: "PRAGMA defer_foreign_keys = ON; SELECT nope FROM missing_table",
            },
        ];

        let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();
        let message = err.to_string();

        assert!(
            message.contains("outside transaction"),
            "unexpected error: {message}"
        );
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

    #[tokio::test]
    async fn pragma_style_migration_keeps_connection_local_state_on_one_connection(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let url = format!("sqlite:{}?mode=rwc", dir.path().join("mig.db").display());
        sqlx::any::install_default_drivers();
        let pool = AnyPoolOptions::new()
            .min_connections(2)
            .max_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(&url)
            .await?;
        let migrations = [Migration {
            version: 1,
            description: "temp table migration",
            sql: "CREATE TEMP TABLE migration_temp (id INTEGER); \
                  INSERT INTO migration_temp VALUES (1); \
                  SELECT * FROM migration_temp",
        }];

        Migrator::new(&pool, &migrations).run().await?;
        assert_eq!(applied_versions(&pool).await?, vec![1]);
        Ok(())
    }

    // --- rewrite_placeholders tests ---

    #[test]
    fn rewrite_placeholders_noop_for_sqlite() {
        let sql = "SELECT * FROM t WHERE id = ? AND name = ?";
        assert_eq!(rewrite_placeholders(sql, Dialect::Sqlite), sql);
    }

    #[test]
    fn rewrite_placeholders_rewrites_for_postgres() {
        let sql = "SELECT * FROM t WHERE id = ? AND name = ?";
        assert_eq!(
            rewrite_placeholders(sql, Dialect::Postgres),
            "SELECT * FROM t WHERE id = $1 AND name = $2"
        );
    }

    #[test]
    fn rewrite_placeholders_skips_string_literals() {
        let sql = "INSERT INTO t (a, b) VALUES (?, 'literal?value')";
        assert_eq!(
            rewrite_placeholders(sql, Dialect::Postgres),
            "INSERT INTO t (a, b) VALUES ($1, 'literal?value')"
        );
    }

    #[test]
    fn rewrite_placeholders_handles_escaped_quotes() {
        let sql = "INSERT INTO t (a) VALUES ('it''s a ?') WHERE x = ?";
        assert_eq!(
            rewrite_placeholders(sql, Dialect::Postgres),
            "INSERT INTO t (a) VALUES ('it''s a ?') WHERE x = $1"
        );
    }

    #[test]
    fn rewrite_placeholders_empty_sql() {
        assert_eq!(rewrite_placeholders("", Dialect::Postgres), "");
    }

    #[test]
    fn rewrite_placeholders_no_params() {
        let sql = "SELECT 1";
        assert_eq!(rewrite_placeholders(sql, Dialect::Postgres), "SELECT 1");
    }
}
