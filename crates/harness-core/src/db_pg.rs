use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::collections::HashSet;
use std::str::FromStr as _;

use crate::db::Migration;

/// Create a Postgres connection pool for the given DATABASE_URL.
///
/// Uses 8 max connections with a 10-second acquire timeout.
pub async fn pg_open_pool(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// Validates that a schema name contains only ASCII letters, digits, and
/// underscores, and starts with a letter or underscore. Rejects the name
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
/// Sets search_path via BOTH the connection `options` startup parameter AND an
/// `after_connect` hook that runs `SET search_path TO <schema>` per connection.
/// The after_connect hook is required for Supabase pgbouncer pooler, which
/// silently strips the startup `options` parameter, causing all DDL to fall
/// back to the default `public` schema and all stores to share a single
/// `schema_migrations` table (migration-number collision bug).
///
/// Returns an error if `schema` contains characters outside `[a-zA-Z0-9_]` or
/// does not start with a letter or underscore — rejecting invalid names before
/// they are interpolated into SQL.
pub async fn pg_open_pool_schematized(database_url: &str, schema: &str) -> anyhow::Result<PgPool> {
    validate_schema_name(schema)?;
    let opts = PgConnectOptions::from_str(database_url)?.options([("search_path", schema)]);
    let schema_for_hook = schema.to_string();
    let pool = PgPoolOptions::new()
        .max_connections(8)
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
    use super::validate_schema_name;

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
}
