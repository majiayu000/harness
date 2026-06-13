use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{Acquire as _, Postgres, Transaction};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Mutex;

use crate::db::Migration;
use crate::db_pg_schema_registry::{
    normalize_path_lexically, register_pg_schema_ownership, PgSchemaOwnership,
};

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
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
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
    ownership: Option<PgSchemaOwnership>,
}

impl std::fmt::Debug for PgStoreContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgStoreContext")
            .field("database_url", &"[REDACTED]")
            .field("schema", &self.schema)
            .field("ownership", &self.ownership)
            .finish()
    }
}

impl PgStoreContext {
    pub fn from_path(path: &Path, configured_database_url: Option<&str>) -> anyhow::Result<Self> {
        let database_url = resolve_database_url(configured_database_url)?;
        let hash_path = schema_hash_path(path)?;
        let schema = pg_schema_for_path(&hash_path)?;
        let ownership = PgSchemaOwnership::path_derived(schema.clone(), hash_path)?;
        Ok(Self::new(database_url, schema)?.with_ownership(ownership))
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
            ownership: None,
        })
    }

    pub fn with_ownership(mut self, ownership: PgSchemaOwnership) -> Self {
        self.ownership = Some(ownership);
        self
    }

    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn ownership(&self) -> Option<&PgSchemaOwnership> {
        self.ownership.as_ref()
    }

    pub async fn ensure_schema(&self, setup_pool: &PgPool) -> anyhow::Result<()> {
        pg_create_schema_if_not_exists(setup_pool, &self.schema)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to ensure Postgres schema '{}' is ready: {error}",
                    self.schema
                )
            })?;
        if let Some(ownership) = &self.ownership {
            register_pg_schema_ownership(setup_pool, ownership)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "failed to register Postgres schema '{}' ownership: {error}",
                        self.schema
                    )
                })?;
        }
        Ok(())
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
pub(crate) fn validate_schema_name(schema: &str) -> anyhow::Result<()> {
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
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(
                hashtextextended('harness_schema_migrations:' || current_schema(), 0)
            )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version     BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                applied_at  TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&mut *tx)
        .await?;

        let rows: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM schema_migrations ORDER BY version ASC")
                .fetch_all(&mut *tx)
                .await?;
        let applied: HashSet<u32> = rows.into_iter().map(|(v,)| v as u32).collect();

        let mut pending: Vec<&Migration> = self
            .migrations
            .iter()
            .filter(|m| !applied.contains(&m.version))
            .collect();
        pending.sort_by_key(|m| m.version);

        for migration in pending {
            Self::apply(&mut tx, migration).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn apply(
        tx: &mut Transaction<'_, Postgres>,
        migration: &Migration,
    ) -> anyhow::Result<()> {
        for stmt in crate::db_pg_split::pg_split_statements(migration.sql) {
            let mut statement_tx = (&mut *tx).begin().await?;
            match sqlx::query(&stmt).execute(&mut *statement_tx).await {
                Ok(_) => {
                    statement_tx.commit().await?;
                }
                Err(e) if pg_duplicate_column_error(&stmt, &e) => {
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
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "db_pg_tests.rs"]
mod tests;
