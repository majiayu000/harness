use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use crate::db::Migration;

const DEFAULT_PG_MAX_CONNECTIONS: u32 = 8;
const SUPABASE_POOLER_MAX_CONNECTIONS: u32 = 1;

fn pg_max_connections(database_url: &str) -> u32 {
    // Supabase's session pooler has a tight session cap relative to the
    // number of independent Postgres-backed stores the workspace tests create.
    // Budget one session per store there to avoid test-time pool starvation.
    if database_url.contains(".pooler.supabase.com") {
        SUPABASE_POOLER_MAX_CONNECTIONS
    } else {
        DEFAULT_PG_MAX_CONNECTIONS
    }
}

/// Resolve the effective Postgres connection string.
///
/// Precedence:
/// 1. Explicit configured URL (for example `server.database_url` from TOML)
/// 2. `HARNESS_DATABASE_URL` config override
/// 3. Discovered Harness config file
/// 4. Repository-local `config/default.toml`
pub fn resolve_database_url(configured_database_url: Option<&str>) -> anyhow::Result<String> {
    if let Some(url) = configured_database_url
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        return Ok(url.to_string());
    }

    if let Some(url) = configured_database_url_from_config()? {
        return Ok(url);
    }

    anyhow::bail!(
        "database URL is not configured; set server.database_url in TOML or HARNESS_DATABASE_URL in the environment"
    )
}

fn configured_database_url_from_config() -> anyhow::Result<Option<String>> {
    if let Some(url) = configured_database_url_from_env("HARNESS_DATABASE_URL") {
        return Ok(Some(url));
    }
    for path in database_url_config_paths()? {
        if let Some(url) = load_database_url_from_path(&path)? {
            return Ok(Some(url));
        }
    }
    Ok(None)
}

fn configured_database_url_from_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
}

fn database_url_config_paths() -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if let Some(path) = crate::config::dirs::find_config_file() {
        paths.push(path);
    }

    if let Some(path) = repository_default_config_from_dir(std::env::current_dir()?) {
        push_unique_path(&mut paths, path);
    }

    // Unit tests may temporarily change the process CWD while other tests open
    // stores concurrently. The test binary path is stable, so this preserves the
    // repository config fallback without reading the generic DATABASE_URL.
    if std::env::var_os("XDG_CONFIG_HOME").is_none() {
        if let Ok(current_exe) = std::env::current_exe() {
            if let Some(parent) = current_exe.parent() {
                if let Some(path) = repository_default_config_from_dir(parent) {
                    push_unique_path(&mut paths, path);
                }
            }
        }
    }
    Ok(paths)
}

fn repository_default_config_from_dir(start: impl AsRef<Path>) -> Option<PathBuf> {
    let mut dir = start.as_ref().to_path_buf();
    loop {
        let local_default = dir.join("config/default.toml");
        if local_default.is_file() {
            return Some(local_default);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn load_database_url_from_path(path: &Path) -> anyhow::Result<Option<String>> {
    #[derive(serde::Deserialize)]
    struct DatabaseConfigFile {
        #[serde(default)]
        server: DatabaseServerConfig,
    }

    #[derive(Default, serde::Deserialize)]
    struct DatabaseServerConfig {
        #[serde(default)]
        database_url: Option<String>,
    }

    let content = std::fs::read_to_string(path)?;
    let config: DatabaseConfigFile = toml::from_str(&content)?;
    Ok(config
        .server
        .database_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(ToOwned::to_owned))
}

/// Create a Postgres connection pool for the given connection string.
///
/// Uses 8 max connections by default, or 1 for Supabase pooler URLs, with a
/// 10-second acquire timeout.
pub async fn pg_open_pool(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(pg_max_connections(database_url))
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
        .max_connections(pg_max_connections(database_url))
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
    use super::{
        pg_max_connections, resolve_database_url, validate_schema_name, DEFAULT_PG_MAX_CONNECTIONS,
    };

    static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct CurrentDirGuard {
        original: std::path::PathBuf,
    }

    impl CurrentDirGuard {
        fn enter(path: &std::path::Path) -> Self {
            let original = std::env::current_dir().expect("current dir");
            std::env::set_current_dir(path).expect("set current dir");
            Self { original }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.original).expect("restore current dir");
        }
    }

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
    fn supabase_pooler_urls_use_single_connection() {
        let url = "postgresql://user:pass@aws-1-ap-northeast-1.pooler.supabase.com:5432/postgres";
        assert_eq!(pg_max_connections(url), 1);
    }

    #[test]
    fn direct_postgres_urls_keep_default_connection_budget() {
        let url = "postgresql://user:pass@db.example.internal:5432/postgres";
        assert_eq!(pg_max_connections(url), DEFAULT_PG_MAX_CONNECTIONS);
    }

    #[test]
    fn configured_database_url_wins_over_config_sources() {
        let _lock = CONFIG_ENV_LOCK.lock().expect("config env lock");
        temp_env::with_vars(
            [
                (
                    "HARNESS_DATABASE_URL",
                    Some("postgres://env-user:env-pass@env-host:5432/envdb"),
                ),
                (
                    "DATABASE_URL",
                    Some("postgres://ignored-user:ignored-pass@ignored-host:5432/ignoreddb"),
                ),
            ],
            || {
                let resolved =
                    resolve_database_url(Some("postgres://cfg-user:cfg-pass@cfg-host:5432/cfgdb"))
                        .expect("configured database URL should resolve");
                assert_eq!(resolved, "postgres://cfg-user:cfg-pass@cfg-host:5432/cfgdb");
            },
        );
    }

    #[test]
    fn harness_database_url_used_as_config_override() {
        let _lock = CONFIG_ENV_LOCK.lock().expect("config env lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let _cwd = CurrentDirGuard::enter(dir.path());
        let xdg = dir.path().join("xdg");
        temp_env::with_vars(
            [
                ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
                ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
                (
                    "HARNESS_DATABASE_URL",
                    Some("postgres://env-user:env-pass@env-host:5432/envdb"),
                ),
                (
                    "DATABASE_URL",
                    Some("postgres://ignored-user:ignored-pass@ignored-host:5432/ignoreddb"),
                ),
            ],
            || {
                let resolved =
                    resolve_database_url(None).expect("HARNESS_DATABASE_URL should resolve");
                assert_eq!(resolved, "postgres://env-user:env-pass@env-host:5432/envdb");
            },
        );
    }

    #[test]
    fn discovered_config_database_url_used_as_fallback() {
        let _lock = CONFIG_ENV_LOCK.lock().expect("config env lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let _cwd = CurrentDirGuard::enter(dir.path());
        let xdg = dir.path().join("xdg");
        let harness_dir = xdg.join("harness");
        std::fs::create_dir_all(&harness_dir).expect("create config dir");

        let mut config = crate::config::HarnessConfig::default();
        config.server.database_url =
            Some("postgres://file-user:file-pass@file-host:5432/filedb".to_string());
        std::fs::write(
            harness_dir.join("config.toml"),
            toml::to_string(&config).expect("serialize config"),
        )
        .expect("write config");

        temp_env::with_vars(
            [
                ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
                ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
                ("HARNESS_DATABASE_URL", None::<&str>),
                (
                    "DATABASE_URL",
                    Some("postgres://ignored-user:ignored-pass@ignored-host:5432/ignoreddb"),
                ),
            ],
            || {
                let resolved =
                    resolve_database_url(None).expect("config database URL should resolve");
                assert_eq!(
                    resolved,
                    "postgres://file-user:file-pass@file-host:5432/filedb"
                );
            },
        );
    }

    #[test]
    fn repository_default_config_database_url_used_as_fallback() {
        let _lock = CONFIG_ENV_LOCK.lock().expect("config env lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let xdg = dir.path().join("xdg");
        let repo_config = dir.path().join("config");
        let nested_cwd = dir.path().join("crates/harness-core");
        std::fs::create_dir_all(&repo_config).expect("create repo config dir");
        std::fs::create_dir_all(&nested_cwd).expect("create nested cwd");
        std::fs::write(
            repo_config.join("default.toml"),
            r#"
                [server]
                database_url = "postgres://repo-user:repo-pass@repo-host:5432/repodb"
            "#,
        )
        .expect("write repo config");

        let _cwd = CurrentDirGuard::enter(&nested_cwd);
        temp_env::with_vars(
            [
                ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
                ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
                ("HARNESS_DATABASE_URL", None::<&str>),
                (
                    "DATABASE_URL",
                    Some("postgres://ignored-user:ignored-pass@ignored-host:5432/ignoreddb"),
                ),
            ],
            || {
                let resolved =
                    resolve_database_url(None).expect("repo config database URL should resolve");
                assert_eq!(
                    resolved,
                    "postgres://repo-user:repo-pass@repo-host:5432/repodb"
                );
            },
        );
    }

    #[test]
    fn bare_database_url_is_ignored_without_config() {
        let _lock = CONFIG_ENV_LOCK.lock().expect("config env lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let _cwd = CurrentDirGuard::enter(dir.path());
        let xdg = dir.path().join("xdg");
        temp_env::with_vars(
            [
                ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
                ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
                ("HARNESS_DATABASE_URL", None::<&str>),
                (
                    "DATABASE_URL",
                    Some("postgres://env-user:env-pass@env-host:5432/envdb"),
                ),
            ],
            || {
                let err =
                    resolve_database_url(None).expect_err("bare DATABASE_URL should not resolve");
                assert!(
                    err.to_string().contains("server.database_url"),
                    "error should mention the TOML config path, got: {err}"
                );
            },
        );
    }

    #[test]
    fn missing_database_url_returns_error() {
        let _lock = CONFIG_ENV_LOCK.lock().expect("config env lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let _cwd = CurrentDirGuard::enter(dir.path());
        let xdg = dir.path().join("xdg");
        temp_env::with_vars(
            [
                ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
                ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
                ("HARNESS_DATABASE_URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || {
                let err = resolve_database_url(None).expect_err("missing database URL should fail");
                assert!(
                    err.to_string().contains("server.database_url"),
                    "error should mention the TOML config path, got: {err}"
                );
            },
        );
    }
}
