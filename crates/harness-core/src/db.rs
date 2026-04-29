use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use std::path::Path;

// Postgres pool helpers and PgMigrator live in the sibling db_pg module.
// Re-export them here so existing callers using `harness_core::db::*` continue
// to work without any import changes.
pub use crate::db_pg::{
    configure_pg_pool_from_server, pg_create_schema_if_not_exists, pg_open_pool,
    pg_open_pool_schematized, pg_schema_for_path, resolve_database_url, PgMigrator, PgStoreContext,
};

/// Create a Postgres connection pool for the logical store at `path`.
///
/// Historical stores accepted SQLite file paths as their identity boundary.
/// The Postgres backend preserves that boundary by hashing each path to a
/// stable schema name.
pub async fn open_pool(path: &Path) -> anyhow::Result<PgPool> {
    PgStoreContext::from_path(path, None)?.open_pool().await
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

/// Generic Postgres store that persists entities as JSON blobs.
///
/// Schema:
/// ```sql
/// CREATE TABLE IF NOT EXISTS <table_name> (
///     id         TEXT PRIMARY KEY,
///     data       TEXT NOT NULL,
///     created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
///     updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
/// )
/// ```
///
/// Use this when entities do not need queryable columns beyond `id`.
/// For entities requiring SQL-level filtering, keep a specialised store and
/// call [`open_pool`] for pool creation.
pub struct Db<T: DbEntity> {
    pub(crate) pool: PgPool,
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
            "INSERT INTO {table} (id, data) VALUES ($1, $2)
             ON CONFLICT(id) DO UPDATE SET data = EXCLUDED.data,
                 updated_at = CURRENT_TIMESTAMP",
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
        let sql = format!("SELECT data FROM {} WHERE id = $1", T::table_name());
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
        let sql = format!("DELETE FROM {} WHERE id = $1", T::table_name());
        let result = sqlx::query(&sql).bind(id).execute(&self.pool).await?;
        Ok(result.rows_affected() > 0)
    }

    /// Expose the underlying pool for stores that need custom queries beyond
    /// the generic CRUD operations.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Trait for types that can be serialized to/from a database status column.
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

/// Backwards-compatible alias for the Postgres migrator.
pub type Migrator<'a> = PgMigrator<'a>;

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
