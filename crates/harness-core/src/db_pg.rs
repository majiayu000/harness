use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::{Arc, OnceLock};

use crate::db::Migration;

const DEFAULT_PG_MAX_CONNECTIONS: u32 = 8;
const DB_TEST_LOCK_STALE_SECS: u64 = 300;
static DB_TEST_PROCESS_LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
static DB_TEST_AVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn db_test_process_lock() -> Arc<tokio::sync::Mutex<()>> {
    DB_TEST_PROCESS_LOCK
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn db_test_lock_path() -> PathBuf {
    std::env::temp_dir().join("harness-db-tests.lock")
}

#[cfg(target_os = "linux")]
fn read_proc_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rfind(')')?;
    let fields: Vec<&str> = stat[after_comm + 1..].split_whitespace().collect();
    fields.get(19)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn db_test_lock_is_stale(lock_path: &Path, meta: &std::fs::Metadata) -> bool {
    if let Ok(content) = std::fs::read_to_string(lock_path) {
        let trimmed = content.trim();
        let (pid_opt, stored_start_opt) = if let Some((pid, start)) = trimmed.split_once(':') {
            (pid.parse::<u32>().ok(), start.parse::<u64>().ok())
        } else {
            (trimmed.parse::<u32>().ok(), None)
        };
        if let Some(pid) = pid_opt {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                return true;
            }
            if let (Some(stored), Some(current)) = (stored_start_opt, read_proc_start_time(pid)) {
                if stored != current {
                    return true;
                }
            }
            return false;
        }
    }
    meta.modified()
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs() > DB_TEST_LOCK_STALE_SECS)
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn db_test_lock_is_stale(_lock_path: &Path, meta: &std::fs::Metadata) -> bool {
    meta.modified()
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs() > DB_TEST_LOCK_STALE_SECS)
        .unwrap_or(false)
}

struct DbTestFileLock {
    path: PathBuf,
}

impl DbTestFileLock {
    fn try_acquire(lock_path: &Path) -> anyhow::Result<Option<Self>> {
        if let Ok(meta) = std::fs::metadata(lock_path) {
            if db_test_lock_is_stale(lock_path, &meta) {
                let _ = std::fs::remove_file(lock_path);
            }
        }

        let pid = std::process::id();
        #[cfg(target_os = "linux")]
        let lock_content = {
            let start = read_proc_start_time(pid).unwrap_or(0);
            format!("{pid}:{start}")
        };
        #[cfg(not(target_os = "linux"))]
        let lock_content = pid.to_string();

        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut file) => {
                let _ = file.write_all(lock_content.as_bytes());
                Ok(Some(Self {
                    path: lock_path.to_path_buf(),
                }))
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

impl Drop for DbTestFileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct DbTestGuard {
    _process_guard: tokio::sync::OwnedMutexGuard<()>,
    _file_lock: DbTestFileLock,
}

pub async fn acquire_db_test_guard() -> anyhow::Result<DbTestGuard> {
    let process_guard = db_test_process_lock().lock_owned().await;
    let lock_path = db_test_lock_path();
    loop {
        if let Some(file_lock) = DbTestFileLock::try_acquire(&lock_path)? {
            return Ok(DbTestGuard {
                _process_guard: process_guard,
                _file_lock: file_lock,
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

pub async fn db_tests_enabled() -> bool {
    if DB_TEST_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }

    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return false;
    };

    let available = match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        pg_open_pool(&database_url),
    )
    .await
    {
        Ok(Ok(pool)) => {
            pool.close().await;
            true
        }
        _ => false,
    };

    if available {
        DB_TEST_AVAILABLE.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    available
}

pub fn is_pool_timeout(err: &anyhow::Error) -> bool {
    err.downcast_ref::<sqlx::Error>()
        .map(|sqlx_err| matches!(sqlx_err, sqlx::Error::PoolTimedOut))
        .unwrap_or(false)
}

/// Resolve the effective Postgres connection string.
///
/// Precedence:
/// 1. Explicit configured URL (for example `server.database_url` from TOML)
/// 2. Legacy `DATABASE_URL` environment variable fallback
pub fn resolve_database_url(configured_database_url: Option<&str>) -> anyhow::Result<String> {
    if let Some(url) = configured_database_url
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        return Ok(url.to_string());
    }
    if let Ok(url) = std::env::var("DATABASE_URL") {
        let url = url.trim();
        if !url.is_empty() {
            return Ok(url.to_string());
        }
    }
    anyhow::bail!(
        "database URL is not configured; set server.database_url in TOML or DATABASE_URL in the environment"
    )
}

/// Create a Postgres connection pool for the given DATABASE_URL.
///
/// Uses 8 max connections with a 10-second acquire timeout.
pub async fn pg_open_pool(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(DEFAULT_PG_MAX_CONNECTIONS)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// Validates that a schema name contains only ASCII letters, digits, and
/// underscores, starts with a letter or underscore, and fits within
/// PostgreSQL's 63-byte identifier limit (NAMEDATALEN-1). Rejects the name
/// before it is ever interpolated into SQL, providing defence in depth against
/// injection if the schema-name construction ever changes.
fn validate_schema_name(schema: &str) -> anyhow::Result<()> {
    let mut chars = schema.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => anyhow::bail!("schema name must not be empty"),
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        anyhow::bail!(
            "schema name must start with a letter or underscore, got: {:?}",
            schema
        );
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        anyhow::bail!(
            "schema name may only contain ASCII letters, digits, and underscores, got: {:?}",
            schema
        );
    }
    // PostgreSQL silently truncates identifiers to 63 bytes (NAMEDATALEN-1).
    // Two names differing only after byte 63 would alias to the same schema.
    if schema.len() > 63 {
        anyhow::bail!(
            "schema name exceeds PostgreSQL's 63-byte identifier limit ({} bytes): {:?}",
            schema.len(),
            schema
        );
    }
    Ok(())
}

/// Creates the Postgres schema `schema` if it does not already exist.
///
/// Validates the schema name before any SQL interpolation — this is the single
/// mandatory entry-point for schema creation so the name is always checked
/// regardless of which store calls it.
pub async fn pg_create_schema_if_not_exists(pool: &PgPool, schema: &str) -> anyhow::Result<()> {
    validate_schema_name(schema)?;
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", schema))
        .execute(pool)
        .await?;
    Ok(())
}

/// Create a Postgres connection pool where every connection has `search_path`
/// set to `schema`. Used to give each store an isolated schema namespace.
///
/// Sets search_path exclusively via an `after_connect` hook that runs
/// `SET search_path TO <schema>` per connection. The hook approach is required
/// for Supabase pgbouncer/Supavisor compatibility: startup `options` parameters
/// are silently stripped by pgbouncer, and unique startup params cause Supavisor
/// to create a separate pool object per schema (hitting EMAXPOOLSREACHED under
/// concurrent test load). The after_connect hook is reliable for all backends.
///
/// Returns an error if `schema` contains characters outside `[a-zA-Z0-9_]` or
/// does not start with a letter or underscore — rejecting invalid names before
/// they are interpolated into SQL.
pub async fn pg_open_pool_schematized(database_url: &str, schema: &str) -> anyhow::Result<PgPool> {
    validate_schema_name(schema)?;
    let opts = PgConnectOptions::from_str(database_url)?;
    let schema_for_hook = schema.to_string();
    let pool = PgPoolOptions::new()
        .max_connections(DEFAULT_PG_MAX_CONNECTIONS)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .after_connect(move |conn, _meta| {
            let schema = schema_for_hook.clone();
            Box::pin(async move {
                let stmt = format!("SET search_path TO \"{}\"", schema);
                sqlx::query(&stmt).execute(conn).await?;
                Ok(())
            })
        })
        .connect_with(opts)
        .await?;
    Ok(pool)
}

fn pg_duplicate_column_error(statement: &str, error: &sqlx::Error) -> bool {
    if !statement.to_ascii_uppercase().contains("ADD COLUMN") {
        return false;
    }
    match error {
        sqlx::Error::Database(db_err) => db_err.code().as_deref() == Some("42701"),
        _ => false,
    }
}

/// Runs versioned SQL migrations against a Postgres pool.
///
/// Maintains a `schema_migrations` table to track which versions have been
/// applied. Safe to call on every startup — already-applied versions are
/// skipped. All migrations run inside a transaction (Postgres supports
/// transactional DDL). Duplicate-column errors on `ALTER TABLE ADD COLUMN`
/// are silently ignored for idempotency.
pub struct PgMigrator<'a> {
    pool: &'a PgPool,
    migrations: &'a [Migration],
}

impl<'a> PgMigrator<'a> {
    pub fn new(pool: &'a PgPool, migrations: &'a [Migration]) -> Self {
        Self { pool, migrations }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version     BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                applied_at  TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(self.pool)
        .await?;

        let rows: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM schema_migrations ORDER BY version ASC")
                .fetch_all(self.pool)
                .await?;
        let applied: HashSet<u32> = rows.into_iter().map(|(v,)| v as u32).collect();

        let mut pending: Vec<&Migration> = self
            .migrations
            .iter()
            .filter(|m| !applied.contains(&m.version))
            .collect();
        pending.sort_by_key(|m| m.version);

        for migration in pending {
            self.apply(migration).await?;
        }
        Ok(())
    }

    async fn apply(&self, migration: &Migration) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for raw in migration.sql.split(';') {
            let stmt = raw.trim();
            if stmt.is_empty() {
                continue;
            }
            if let Err(e) = sqlx::query(stmt).execute(&mut *tx).await {
                if pg_duplicate_column_error(stmt, &e) {
                    continue;
                }
                return Err(anyhow::anyhow!(
                    "migration v{} '{}' failed: {} [sql: {}]",
                    migration.version,
                    migration.description,
                    e,
                    stmt
                ));
            }
        }
        sqlx::query("INSERT INTO schema_migrations (version, description) VALUES ($1, $2)")
            .bind(migration.version as i64)
            .bind(migration.description)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_database_url, validate_schema_name, DEFAULT_PG_MAX_CONNECTIONS};

    #[test]
    fn valid_schema_names() {
        for name in ["public", "_priv", "h1a2b3c4d5e6f7a8", "schema_1", "A"] {
            assert!(
                validate_schema_name(name).is_ok(),
                "expected ok for {name:?}"
            );
        }
    }

    #[test]
    fn empty_schema_rejected() {
        assert!(validate_schema_name("").is_err());
    }

    #[test]
    fn digit_first_rejected() {
        assert!(validate_schema_name("1schema").is_err());
    }

    #[test]
    fn double_quote_rejected() {
        assert!(validate_schema_name("sch\"ema").is_err());
    }

    #[test]
    fn semicolon_rejected() {
        assert!(validate_schema_name("s;DROP TABLE").is_err());
    }

    #[test]
    fn hyphen_rejected() {
        assert!(validate_schema_name("my-schema").is_err());
    }

    #[test]
    fn exactly_63_bytes_accepted() {
        let name = "a".repeat(63);
        assert!(
            validate_schema_name(&name).is_ok(),
            "63-byte name should be valid"
        );
    }

    #[test]
    fn over_63_bytes_rejected() {
        let name = "a".repeat(64);
        assert!(
            validate_schema_name(&name).is_err(),
            "64-byte name should be rejected"
        );
    }

    #[test]
    fn postgres_pool_budget_stays_at_default() {
        assert_eq!(DEFAULT_PG_MAX_CONNECTIONS, 8);
    }

    #[test]
    fn configured_database_url_wins_over_environment() {
        temp_env::with_vars(
            [(
                "DATABASE_URL",
                Some("postgres://env-user:env-pass@env-host:5432/envdb"),
            )],
            || {
                let resolved =
                    resolve_database_url(Some("postgres://cfg-user:cfg-pass@cfg-host:5432/cfgdb"))
                        .expect("configured database URL should resolve");
                assert_eq!(resolved, "postgres://cfg-user:cfg-pass@cfg-host:5432/cfgdb");
            },
        );
    }

    #[test]
    fn environment_database_url_used_as_fallback() {
        temp_env::with_vars(
            [(
                "DATABASE_URL",
                Some("postgres://env-user:env-pass@env-host:5432/envdb"),
            )],
            || {
                let resolved =
                    resolve_database_url(None).expect("environment database URL should resolve");
                assert_eq!(resolved, "postgres://env-user:env-pass@env-host:5432/envdb");
            },
        );
    }

    #[test]
    fn missing_database_url_returns_error() {
        temp_env::with_vars([("DATABASE_URL", None::<&str>)], || {
            let err = resolve_database_url(None).expect_err("missing database URL should fail");
            assert!(
                err.to_string().contains("server.database_url"),
                "error should mention the TOML config path, got: {err}"
            );
        });
    }
}
