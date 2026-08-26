use super::validate_identifier;
use crate::db::Migration;
use sqlx::postgres::{PgConnection, PgPool};
use sqlx::Acquire as _;
use std::collections::HashSet;

fn pg_duplicate_column_error(statement: &str, error: &sqlx::Error) -> bool {
    if !statement.to_ascii_uppercase().contains("ADD COLUMN") {
        return false;
    }
    match error {
        sqlx::Error::Database(db_err) => db_err.code().as_deref() == Some("42701"),
        _ => false,
    }
}

pub(super) fn reject_newer_applied_migrations(
    applied: &HashSet<u32>,
    known: &[Migration],
) -> anyhow::Result<()> {
    let newest_applied = applied.iter().copied().max().unwrap_or(0);
    let newest_known = known
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0);
    if newest_applied > newest_known {
        anyhow::bail!(
            "database schema migration v{newest_applied} is newer than this binary supports (latest known: v{newest_known})"
        );
    }
    Ok(())
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
    migration_table: &'a str,
}

impl<'a> PgMigrator<'a> {
    pub fn new(pool: &'a PgPool, migrations: &'a [Migration]) -> Self {
        Self {
            pool,
            migrations,
            migration_table: "schema_migrations",
        }
    }

    /// Use a store-specific migration ledger when multiple logical stores share
    /// one PostgreSQL schema and their migration version numbers would
    /// otherwise collide in the default `schema_migrations` table.
    pub fn new_with_table(
        pool: &'a PgPool,
        migrations: &'a [Migration],
        migration_table: &'a str,
    ) -> anyhow::Result<Self> {
        validate_identifier(migration_table)?;
        Ok(Self {
            pool,
            migrations,
            migration_table,
        })
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let mut conn = self.pool.acquire().await?;
        // Keep the schema-level lock and migration work on one connection; some
        // deployments intentionally run store pools with a single session.
        sqlx::query(
            "SELECT pg_advisory_lock(hashtext(current_database()), hashtext(current_schema()))",
        )
        .execute(&mut *conn)
        .await?;

        let result = self.run_locked(&mut conn).await;
        let unlock_result: Result<bool, sqlx::Error> = sqlx::query_scalar(
            "SELECT pg_advisory_unlock(hashtext(current_database()), hashtext(current_schema()))",
        )
        .fetch_one(&mut *conn)
        .await;

        match (result, unlock_result) {
            (Ok(()), Ok(true)) => Ok(()),
            (Ok(()), Ok(false)) => Err(anyhow::anyhow!(
                "Postgres migration advisory lock was not held at release time"
            )),
            (Ok(()), Err(error)) => Err(anyhow::anyhow!(
                "failed to release Postgres migration advisory lock: {error}"
            )),
            (Err(error), Ok(true)) => Err(error),
            (Err(error), Ok(false)) => Err(anyhow::anyhow!(
                "{error}; Postgres migration advisory lock was not held at release time"
            )),
            (Err(error), Err(unlock_error)) => Err(anyhow::anyhow!(
                "{error}; additionally failed to release Postgres migration advisory lock: {unlock_error}"
            )),
        }
    }

    async fn run_locked(&self, conn: &mut PgConnection) -> anyhow::Result<()> {
        let create_migrations_table = format!(
            "CREATE TABLE IF NOT EXISTS \"{}\" (
                version     BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                applied_at  TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
            self.migration_table
        );
        sqlx::query(&create_migrations_table)
            .execute(&mut *conn)
            .await?;

        let select_versions = format!(
            "SELECT version FROM \"{}\" ORDER BY version ASC",
            self.migration_table
        );
        let rows: Vec<(i64,)> = sqlx::query_as(&select_versions)
            .fetch_all(&mut *conn)
            .await?;
        let applied: HashSet<u32> = rows.into_iter().map(|(v,)| v as u32).collect();
        reject_newer_applied_migrations(&applied, self.migrations)?;

        let mut pending: Vec<&Migration> = self
            .migrations
            .iter()
            .filter(|migration| !applied.contains(&migration.version))
            .collect();
        pending.sort_by_key(|migration| migration.version);

        for migration in pending {
            self.apply(conn, migration).await?;
        }
        Ok(())
    }

    async fn apply(&self, conn: &mut PgConnection, migration: &Migration) -> anyhow::Result<()> {
        let mut tx = conn.begin().await?;
        for statement in crate::db_pg_split::pg_split_statements(migration.sql) {
            let mut statement_tx = (&mut tx).begin().await?;
            match sqlx::query(&statement).execute(&mut *statement_tx).await {
                Ok(_) => statement_tx.commit().await?,
                Err(error) if pg_duplicate_column_error(&statement, &error) => {
                    statement_tx.rollback().await?;
                    continue;
                }
                Err(error) => {
                    let _ = statement_tx.rollback().await;
                    return Err(anyhow::anyhow!(
                        "migration v{} '{}' failed: {} [sql: {}]",
                        migration.version,
                        migration.description,
                        error,
                        statement
                    ));
                }
            }
        }
        let insert_migration = format!(
            "INSERT INTO \"{}\" (version, description) VALUES ($1, $2)",
            self.migration_table
        );
        sqlx::query(&insert_migration)
            .bind(migration.version as i64)
            .bind(migration.description)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}
