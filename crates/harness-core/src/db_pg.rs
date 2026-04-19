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

/// Create a Postgres connection pool where every connection has `search_path`
/// set to `schema` via connection startup options. Used in test builds to give
/// each test its own isolated schema namespace.
pub async fn pg_open_pool_schematized(database_url: &str, schema: &str) -> anyhow::Result<PgPool> {
    let opts = PgConnectOptions::from_str(database_url)?.options([("search_path", schema)]);
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(10))
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
