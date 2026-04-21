use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::collections::HashSet;
use std::str::FromStr as _;

use crate::db::Migration;

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
/// Uses 3 max connections with a 10-second acquire timeout.
pub async fn pg_open_pool(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(3)
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
        .max_connections(3)
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
    use super::{resolve_database_url, validate_schema_name};

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
