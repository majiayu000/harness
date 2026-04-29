use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Acquire as _;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Mutex;

use crate::db::Migration;

const DEFAULT_PG_MAX_CONNECTIONS: u32 = 8;
const DEFAULT_PG_ACQUIRE_TIMEOUT_SECS: u64 = 10;
const SUPABASE_POOLER_MAX_CONNECTIONS: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PgPoolSettings {
    max_connections: u32,
    acquire_timeout: std::time::Duration,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PgPoolConfig {
    max_connections: Option<u32>,
    acquire_timeout_secs: Option<u64>,
}

impl PgPoolConfig {
    fn apply_missing_from(&mut self, other: Self) {
        if self.max_connections.is_none() {
            self.max_connections = other.max_connections;
        }
        if self.acquire_timeout_secs.is_none() {
            self.acquire_timeout_secs = other.acquire_timeout_secs;
        }
    }
}

static PG_POOL_CONFIG_OVERRIDE: Mutex<PgPoolConfig> = Mutex::new(PgPoolConfig {
    max_connections: None,
    acquire_timeout_secs: None,
});

/// Install server-level Postgres pool settings for subsequently opened stores.
///
/// Store constructors intentionally receive only the database URL for
/// backwards compatibility, so the CLI calls this once after loading the
/// server config. Environment variables are still read at pool-open time and
/// remain the highest-precedence override.
pub fn configure_pg_pool_from_server(server: &crate::config::server::ServerConfig) {
    let mut config = PG_POOL_CONFIG_OVERRIDE
        .lock()
        .expect("Postgres pool config mutex poisoned");
    config.max_connections = server.database_pool_max_connections;
    config.acquire_timeout_secs = server.database_pool_acquire_timeout_secs;
}

fn pg_pool_config_override() -> PgPoolConfig {
    *PG_POOL_CONFIG_OVERRIDE
        .lock()
        .expect("Postgres pool config mutex poisoned")
}

fn default_pg_max_connections(database_url: &str) -> u32 {
    // Supabase's session pooler has a tight session cap relative to the
    // number of independent Postgres-backed stores the workspace tests create.
    // Budget one session per store there to avoid test-time pool starvation.
    if database_url.contains(".pooler.supabase.com") {
        SUPABASE_POOLER_MAX_CONNECTIONS
    } else {
        DEFAULT_PG_MAX_CONNECTIONS
    }
}

fn pg_pool_settings(database_url: &str) -> anyhow::Result<PgPoolSettings> {
    let config = configured_pg_pool_config()?;
    let max_connections = config
        .max_connections
        .unwrap_or_else(|| default_pg_max_connections(database_url));
    if max_connections == 0 {
        anyhow::bail!("database_pool_max_connections must be greater than 0");
    }

    let acquire_timeout_secs = config
        .acquire_timeout_secs
        .unwrap_or(DEFAULT_PG_ACQUIRE_TIMEOUT_SECS);
    if acquire_timeout_secs == 0 {
        anyhow::bail!("database_pool_acquire_timeout_secs must be greater than 0");
    }

    Ok(PgPoolSettings {
        max_connections,
        acquire_timeout: std::time::Duration::from_secs(acquire_timeout_secs),
    })
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
        if let Some(url) = load_database_config_from_path(&path)?.database_url {
            return Ok(Some(url));
        }
    }
    Ok(None)
}

fn configured_pg_pool_config() -> anyhow::Result<PgPoolConfig> {
    let mut config = PgPoolConfig::default();
    for path in database_url_config_paths()? {
        config.apply_missing_from(load_database_config_from_path(&path)?.pool);
    }
    let override_config = pg_pool_config_override();
    if let Some(max_connections) = override_config.max_connections {
        config.max_connections = Some(max_connections);
    }
    if let Some(acquire_timeout_secs) = override_config.acquire_timeout_secs {
        config.acquire_timeout_secs = Some(acquire_timeout_secs);
    }
    if let Some(max_connections) = configured_u32_from_env("HARNESS_DATABASE_POOL_MAX_CONNECTIONS")?
    {
        config.max_connections = Some(max_connections);
    }
    if let Some(acquire_timeout_secs) =
        configured_u64_from_env("HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS")?
    {
        config.acquire_timeout_secs = Some(acquire_timeout_secs);
    }
    Ok(config)
}

fn configured_database_url_from_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
}

fn configured_u32_from_env(name: &str) -> anyhow::Result<Option<u32>> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value
            .trim()
            .parse()
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{name}={value:?} is not a valid u32: {e}")),
        _ => Ok(None),
    }
}

fn configured_u64_from_env(name: &str) -> anyhow::Result<Option<u64>> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value
            .trim()
            .parse()
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{name}={value:?} is not a valid u64: {e}")),
        _ => Ok(None),
    }
}

fn database_url_config_paths() -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if let Some(path) = crate::config::dirs::find_config_file() {
        paths.push(path);
    }

    if let Ok(current_dir) = std::env::current_dir() {
        if let Some(path) = repository_default_config_from_dir(current_dir) {
            push_unique_path(&mut paths, path);
        }
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

struct LoadedDatabaseConfig {
    database_url: Option<String>,
    pool: PgPoolConfig,
}

fn load_database_config_from_path(path: &Path) -> anyhow::Result<LoadedDatabaseConfig> {
    #[derive(serde::Deserialize)]
    struct DatabaseConfigFile {
        #[serde(default)]
        server: DatabaseServerConfig,
    }

    #[derive(Default, serde::Deserialize)]
    struct DatabaseServerConfig {
        #[serde(default)]
        database_url: Option<String>,
        #[serde(default)]
        database_pool_max_connections: Option<u32>,
        #[serde(default)]
        database_pool_acquire_timeout_secs: Option<u64>,
    }

    let content = std::fs::read_to_string(path)?;
    let config: DatabaseConfigFile = toml::from_str(&content)?;
    Ok(LoadedDatabaseConfig {
        database_url: config
            .server
            .database_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(ToOwned::to_owned),
        pool: PgPoolConfig {
            max_connections: config.server.database_pool_max_connections,
            acquire_timeout_secs: config.server.database_pool_acquire_timeout_secs,
        },
    })
}

fn schema_hash_path(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    let absolute = std::env::current_dir()?.join(path);
    if let Some(parent) = absolute.parent() {
        if let Ok(canonical_parent) = parent.canonicalize() {
            if let Some(file_name) = absolute.file_name() {
                return Ok(canonical_parent.join(file_name));
            }
            return Ok(canonical_parent);
        }
    }

    Ok(normalize_path_lexically(&absolute))
}

fn normalize_path_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

/// Derive the legacy per-store Postgres schema name from a store identity path.
///
/// Historical SQLite-backed stores accepted a database file path. The Postgres
/// backend preserves that identity boundary by hashing the path into a stable
/// schema name, so existing data remains addressable after this helper is used
/// by all stores.
pub fn pg_schema_for_path(path: &Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};

    let hash_path = schema_hash_path(path)?;
    let path_utf8 = hash_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {:?}", hash_path))?;
    let digest = Sha256::digest(path_utf8.as_bytes());
    let mut schema_bytes = [0u8; 8];
    schema_bytes.copy_from_slice(&digest[..8]);
    Ok(format!("h{:016x}", u64::from_le_bytes(schema_bytes)))
}

/// Postgres context for a single logical store.
///
/// This centralizes the strategy that used to be repeated by every store:
/// resolve the configured URL, derive or validate the schema, create the
/// schema, open a schematized pool, and optionally run migrations.
#[derive(Clone)]
pub struct PgStoreContext {
    database_url: String,
    schema: String,
}

impl std::fmt::Debug for PgStoreContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgStoreContext")
            .field("database_url", &"[REDACTED]")
            .field("schema", &self.schema)
            .finish()
    }
}

impl PgStoreContext {
    pub fn from_path(path: &Path, configured_database_url: Option<&str>) -> anyhow::Result<Self> {
        let database_url = resolve_database_url(configured_database_url)?;
        Self::new(database_url, pg_schema_for_path(path)?)
    }

    pub fn from_schema(
        schema: &str,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let database_url = resolve_database_url(configured_database_url)?;
        Self::new(database_url, schema.to_string())
    }

    pub fn new(database_url: impl Into<String>, schema: impl Into<String>) -> anyhow::Result<Self> {
        let database_url = database_url.into().trim().to_string();
        if database_url.is_empty() {
            anyhow::bail!("database URL must not be empty");
        }
        let schema = schema.into();
        validate_schema_name(&schema)?;
        Ok(Self {
            database_url,
            schema,
        })
    }

    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub async fn ensure_schema(&self, setup_pool: &PgPool) -> anyhow::Result<()> {
        pg_create_schema_if_not_exists(setup_pool, &self.schema)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to ensure Postgres schema '{}' is ready: {error}",
                    self.schema
                )
            })
    }

    pub async fn open_runtime_pool(&self) -> anyhow::Result<PgPool> {
        pg_open_pool_schematized(&self.database_url, &self.schema)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to open Postgres runtime pool for schema '{}': {error}",
                    self.schema
                )
            })
    }

    pub async fn open_pool_with_setup_pool(&self, setup_pool: &PgPool) -> anyhow::Result<PgPool> {
        self.ensure_schema(setup_pool).await?;
        self.open_runtime_pool().await
    }

    pub async fn open_migrated_pool_with_setup_pool(
        &self,
        setup_pool: &PgPool,
        migrations: &[Migration],
    ) -> anyhow::Result<PgPool> {
        let pool = self.open_pool_with_setup_pool(setup_pool).await?;
        PgMigrator::new(&pool, migrations)
            .run()
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to run Postgres migrations for schema '{}': {error}",
                    self.schema
                )
            })?;
        Ok(pool)
    }

    pub async fn open_pool(&self) -> anyhow::Result<PgPool> {
        let setup = pg_open_pool(&self.database_url).await.map_err(|error| {
            anyhow::anyhow!(
                "failed to open Postgres bootstrap pool for schema '{}': {error}",
                self.schema
            )
        })?;
        let pool = self.open_pool_with_setup_pool(&setup).await;
        setup.close().await;
        pool
    }

    pub async fn open_migrated_pool(&self, migrations: &[Migration]) -> anyhow::Result<PgPool> {
        let setup = pg_open_pool(&self.database_url).await.map_err(|error| {
            anyhow::anyhow!(
                "failed to open Postgres bootstrap pool for schema '{}': {error}",
                self.schema
            )
        })?;
        let pool = self
            .open_migrated_pool_with_setup_pool(&setup, migrations)
            .await;
        setup.close().await;
        pool
    }
}

/// Create a Postgres connection pool for the given connection string.
///
/// Uses a configurable max connection budget, with a Supabase-specific
/// single-session default for pooler URLs.
pub async fn pg_open_pool(database_url: &str) -> anyhow::Result<PgPool> {
    let settings = pg_pool_settings(database_url)?;
    let pool = PgPoolOptions::new()
        .max_connections(settings.max_connections)
        .acquire_timeout(settings.acquire_timeout)
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
    let settings = pg_pool_settings(database_url)?;
    let schema_for_hook = schema.to_string();
    let pool = PgPoolOptions::new()
        .max_connections(settings.max_connections)
        .acquire_timeout(settings.acquire_timeout)
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
            let mut statement_tx = (&mut tx).begin().await?;
            match sqlx::query(stmt).execute(&mut *statement_tx).await {
                Ok(_) => {
                    statement_tx.commit().await?;
                }
                Err(e) if pg_duplicate_column_error(stmt, &e) => {
                    statement_tx.rollback().await?;
                    continue;
                }
                Err(e) => {
                    let _ = statement_tx.rollback().await;
                    return Err(anyhow::anyhow!(
                        "migration v{} '{}' failed: {} [sql: {}]",
                        migration.version,
                        migration.description,
                        e,
                        stmt
                    ));
                }
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
        configure_pg_pool_from_server, pg_open_pool, pg_pool_settings, pg_schema_for_path,
        resolve_database_url, validate_schema_name, PgStoreContext,
        DEFAULT_PG_ACQUIRE_TIMEOUT_SECS, DEFAULT_PG_MAX_CONNECTIONS,
    };
    use crate::config::server::ServerConfig;
    use crate::test_support::process_env_lock;
    use std::path::Path;

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

    fn with_isolated_pool_config<F>(f: F)
    where
        F: FnOnce(),
    {
        let _lock = process_env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _cwd = CurrentDirGuard::enter(dir.path());
        let xdg = dir.path().join("xdg");
        let server = ServerConfig {
            database_pool_max_connections: None,
            database_pool_acquire_timeout_secs: None,
            ..Default::default()
        };
        configure_pg_pool_from_server(&server);
        temp_env::with_vars(
            [
                ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
                ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
                ("HARNESS_DATABASE_POOL_MAX_CONNECTIONS", None::<&str>),
                ("HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS", None::<&str>),
            ],
            || {
                f();
                configure_pg_pool_from_server(&ServerConfig::default());
            },
        );
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
    fn pg_schema_for_path_preserves_legacy_hash_format() {
        let schema = pg_schema_for_path(Path::new("/tmp/harness/tasks.db"))
            .expect("path schema should resolve");

        assert_eq!(schema, "h1b76aa87802f7705");
    }

    #[test]
    fn pg_schema_for_path_normalizes_relative_aliases() {
        let _lock = process_env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let app_dir = dir.path().join("app");
        std::fs::create_dir(&app_dir).expect("create app dir");
        let _cwd = CurrentDirGuard::enter(&app_dir);

        let canonical_path = app_dir
            .canonicalize()
            .expect("canonical app dir")
            .join("tasks.db");
        let canonical_schema =
            pg_schema_for_path(&canonical_path).expect("canonical path schema should resolve");

        assert_eq!(
            pg_schema_for_path(Path::new("./tasks.db")).expect("dot path schema should resolve"),
            canonical_schema
        );
        assert_eq!(
            pg_schema_for_path(Path::new("../app/tasks.db"))
                .expect("parent alias schema should resolve"),
            canonical_schema
        );
    }

    #[test]
    fn pg_store_context_from_path_uses_configured_url_and_path_schema() {
        let context = PgStoreContext::from_path(
            Path::new("/tmp/harness/tasks.db"),
            Some(" postgres://user:pass@localhost:5432/harness "),
        )
        .expect("store context should resolve");

        assert_eq!(
            context.database_url(),
            "postgres://user:pass@localhost:5432/harness"
        );
        assert_eq!(context.schema(), "h1b76aa87802f7705");
    }

    #[test]
    fn pg_store_context_debug_redacts_database_url() {
        let context = PgStoreContext::new(
            "postgres://user:secret@localhost:5432/harness",
            "h1b76aa87802f7705",
        )
        .expect("store context should resolve");
        let debug = format!("{context:?}");

        assert!(debug.contains("database_url: \"[REDACTED]\""));
        assert!(debug.contains("schema: \"h1b76aa87802f7705\""));
        assert!(
            !debug.contains("secret"),
            "debug output must not expose database credentials: {debug}"
        );
    }

    #[test]
    fn pg_store_context_rejects_invalid_schema() {
        let err = PgStoreContext::new("postgres://user:pass@localhost:5432/harness", "bad-schema")
            .expect_err("invalid schema should fail");

        assert!(
            err.to_string().contains("ASCII letters"),
            "error should explain schema validation, got: {err}"
        );
    }

    #[tokio::test]
    async fn pg_store_context_runtime_pool_errors_name_the_step() {
        let context = PgStoreContext::new("not-a-valid-postgres-url", "h1b76aa87802f7705")
            .expect("context should accept opaque URL strings");
        let err = context
            .open_runtime_pool()
            .await
            .expect_err("invalid URL should fail before connecting");
        assert!(
            err.to_string().contains("runtime pool"),
            "error should identify the runtime-pool step: {err}"
        );
    }

    #[test]
    fn pg_store_context_can_reuse_shared_setup_pool() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let _lock = process_env_lock();
        let Ok(database_url) = resolve_database_url(None) else {
            return;
        };
        runtime.block_on(async {
            let setup_pool = match pg_open_pool(&database_url).await {
                Ok(pool) => pool,
                Err(_) => return,
            };
            let context =
                PgStoreContext::from_path(Path::new("/tmp/harness/shared.db"), Some(&database_url))
                    .expect("context should resolve");
            let pool = context
                .open_pool_with_setup_pool(&setup_pool)
                .await
                .expect("shared setup pool should initialize the store");
            pool.close().await;
            setup_pool.close().await;
        });
    }

    #[test]
    fn supabase_pooler_urls_use_single_connection() {
        with_isolated_pool_config(|| {
            let url =
                "postgresql://user:pass@aws-1-ap-northeast-1.pooler.supabase.com:5432/postgres";
            let settings = pg_pool_settings(url).expect("pool settings should resolve");
            assert_eq!(settings.max_connections, 1);
        });
    }

    #[test]
    fn direct_postgres_urls_keep_default_connection_budget() {
        with_isolated_pool_config(|| {
            let url = "postgresql://user:pass@db.example.internal:5432/postgres";
            let settings = pg_pool_settings(url).expect("pool settings should resolve");
            assert_eq!(settings.max_connections, DEFAULT_PG_MAX_CONNECTIONS);
            assert_eq!(
                settings.acquire_timeout,
                std::time::Duration::from_secs(DEFAULT_PG_ACQUIRE_TIMEOUT_SECS)
            );
        });
    }

    #[test]
    fn server_pool_config_overrides_default_connection_budget() {
        with_isolated_pool_config(|| {
            let server = ServerConfig {
                database_pool_max_connections: Some(8),
                database_pool_acquire_timeout_secs: Some(30),
                ..Default::default()
            };
            configure_pg_pool_from_server(&server);

            let settings = pg_pool_settings("postgres://user:pass@localhost:5432/harness")
                .expect("pool settings should resolve");
            assert_eq!(settings.max_connections, 8);
            assert_eq!(settings.acquire_timeout, std::time::Duration::from_secs(30));
        });
    }

    #[test]
    fn env_pool_config_overrides_server_config() {
        with_isolated_pool_config(|| {
            let server = ServerConfig {
                database_pool_max_connections: Some(4),
                database_pool_acquire_timeout_secs: Some(20),
                ..Default::default()
            };
            configure_pg_pool_from_server(&server);

            temp_env::with_vars(
                [
                    ("HARNESS_DATABASE_POOL_MAX_CONNECTIONS", Some("12")),
                    ("HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS", Some("45")),
                ],
                || {
                    let settings = pg_pool_settings("postgres://user:pass@localhost:5432/harness")
                        .expect("pool settings should resolve");
                    assert_eq!(settings.max_connections, 12);
                    assert_eq!(settings.acquire_timeout, std::time::Duration::from_secs(45));
                },
            );
        });
    }

    #[test]
    fn zero_pool_config_is_rejected() {
        with_isolated_pool_config(|| {
            let server = ServerConfig {
                database_pool_max_connections: Some(0),
                ..Default::default()
            };
            configure_pg_pool_from_server(&server);

            let err = pg_pool_settings("postgres://user:pass@localhost:5432/harness")
                .expect_err("zero max connections should fail");
            assert!(
                err.to_string().contains("greater than 0"),
                "error should explain invalid pool size: {err}"
            );
        });
    }

    #[test]
    fn configured_database_url_wins_over_config_sources() {
        let _lock = process_env_lock();
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
        let _lock = process_env_lock();
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
        let _lock = process_env_lock();
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
        let _lock = process_env_lock();
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
    fn discovered_config_survives_deleted_current_dir() {
        let _lock = process_env_lock();
        let home = tempfile::tempdir().expect("home tempdir");
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let xdg = home.path().join("xdg");
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

        let cwd_path = cwd.path().to_path_buf();
        let _cwd = CurrentDirGuard::enter(&cwd_path);
        drop(cwd);

        temp_env::with_vars(
            [
                ("HOME", Some(home.path().to_str().expect("utf8 home"))),
                ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
                ("HARNESS_DATABASE_URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
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
    fn bare_database_url_is_ignored_without_config() {
        let _lock = process_env_lock();
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
        let _lock = process_env_lock();
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
