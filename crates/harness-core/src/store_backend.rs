//! Storage backend seam.
//!
//! Decouples *where a store lives* ([`StoreLocation`]) from *how its handle is
//! opened* ([`Backend`] → [`StoreHandle`]), so the rest of the system can
//! construct stores through one seam instead of calling `PgStoreContext` /
//! `pg_schema_for_path` directly. Part of the storage-layer redesign
//! (`docs/rfc-storage-layer-redesign.md`).
//!
//! Two backends:
//!   - [`PostgresBackend`] — durable, shared orchestration state. Resolves a
//!     [`StoreLocation::SharedSchema`] (bounded, fixed schema) or the legacy
//!     [`StoreLocation::PathDerivedSchema`] (one schema per path — the coupling
//!     that caused the ~539k-schema explosion, slated for removal).
//!   - [`LocalFileBackend`] — ephemeral per-workspace scratch. Opens a SQLite
//!     file at a [`StoreLocation::LocalFile`] path. Files are created cheaply and
//!     deleted with the workspace, so they never create a Postgres schema.
//!
//! This module (Phase 3a) delivers the backend infrastructure. Existing stores
//! still open Postgres directly; migrating ephemeral stores onto
//! [`LocalFileBackend`] happens in follow-up changes.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

use crate::db::{Migration, PgStoreContext};

/// Logical location of a store, independent of the physical backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StoreLocation {
    /// Durable, shared: a fixed, named Postgres schema (bounded count).
    SharedSchema(String),
    /// Legacy: schema derived by hashing a store-identity path
    /// (`pg_schema_for_path`). Produces one schema per distinct path — the
    /// source of the schema explosion. Retained only until callers migrate to
    /// [`StoreLocation::SharedSchema`] or [`StoreLocation::LocalFile`].
    PathDerivedSchema(PathBuf),
    /// Ephemeral, local SQLite file (typically under a workspace directory).
    LocalFile(PathBuf),
}

/// An opened, migrated store handle. The concrete pool type depends on the
/// backend the location routed to.
#[derive(Debug, Clone)]
pub enum StoreHandle {
    /// Durable Postgres pool (schema-scoped via `search_path`).
    Postgres(PgPool),
    /// Ephemeral SQLite pool backed by a local file.
    Sqlite(SqlitePool),
}

impl StoreHandle {
    /// Borrow the Postgres pool, if this handle is Postgres-backed.
    pub fn as_postgres(&self) -> Option<&PgPool> {
        match self {
            StoreHandle::Postgres(pool) => Some(pool),
            StoreHandle::Sqlite(_) => None,
        }
    }

    /// Borrow the SQLite pool, if this handle is file-backed.
    pub fn as_sqlite(&self) -> Option<&SqlitePool> {
        match self {
            StoreHandle::Sqlite(pool) => Some(pool),
            StoreHandle::Postgres(_) => None,
        }
    }
}

/// Opens (and migrates) a store for a given [`StoreLocation`].
///
/// `migrations` must be written in the SQL dialect of the backend the location
/// routes to (Postgres for schema locations, SQLite for [`StoreLocation::LocalFile`]).
#[async_trait]
pub trait Backend: Send + Sync {
    /// Open a migrated store handle for `loc`.
    async fn open_migrated(
        &self,
        loc: &StoreLocation,
        migrations: &[Migration],
    ) -> anyhow::Result<StoreHandle>;
}

/// Postgres-backed implementation that wraps the existing `PgStoreContext`.
#[derive(Debug, Clone, Default)]
pub struct PostgresBackend {
    configured_database_url: Option<String>,
}

impl PostgresBackend {
    /// Create a backend that resolves the database URL the same way the rest of
    /// the system does (explicit override, else env/config).
    pub fn new(configured_database_url: Option<String>) -> Self {
        Self {
            configured_database_url,
        }
    }

    /// Resolve the `PgStoreContext` for a Postgres `loc`.
    ///
    /// Centralizes the schema strategy (shared vs path-derived) in one place, so
    /// callers no longer compute `pg_schema_for_path` inline. Builders that open
    /// against a shared setup pool (`*_with_context`) use this directly; errors
    /// for [`StoreLocation::LocalFile`] (use [`LocalFileBackend`] for those).
    pub fn store_context(&self, loc: &StoreLocation) -> anyhow::Result<PgStoreContext> {
        let url = self.configured_database_url.as_deref();
        match loc {
            StoreLocation::SharedSchema(schema) => PgStoreContext::from_schema(schema, url),
            StoreLocation::PathDerivedSchema(path) => {
                PgStoreContext::from_legacy_path_schema(path, url)
            }
            StoreLocation::LocalFile(path) => Err(anyhow::anyhow!(
                "PostgresBackend cannot open a LocalFile location ({}); use LocalFileBackend",
                path.display()
            )),
        }
    }
}

#[async_trait]
impl Backend for PostgresBackend {
    async fn open_migrated(
        &self,
        loc: &StoreLocation,
        migrations: &[Migration],
    ) -> anyhow::Result<StoreHandle> {
        let pool = self
            .store_context(loc)?
            .open_migrated_pool(migrations)
            .await?;
        Ok(StoreHandle::Postgres(pool))
    }
}

/// SQLite-backed implementation for ephemeral, per-workspace local files.
///
/// Opens (creating if missing) a SQLite database at the [`StoreLocation::LocalFile`]
/// path and applies SQLite-dialect migrations. No Postgres schema is created, so
/// these stores cannot contribute to catalog bloat.
#[derive(Debug, Clone, Default)]
pub struct LocalFileBackend;

impl LocalFileBackend {
    pub fn new() -> Self {
        Self
    }

    /// Open a migrated SQLite pool at `path`, creating parent dirs and the file.
    pub async fn open_sqlite_migrated(
        path: PathBuf,
        migrations: &[Migration],
    ) -> anyhow::Result<SqlitePool> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(30));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        run_sqlite_migrations(&pool, migrations).await?;
        Ok(pool)
    }
}

#[async_trait]
impl Backend for LocalFileBackend {
    async fn open_migrated(
        &self,
        loc: &StoreLocation,
        migrations: &[Migration],
    ) -> anyhow::Result<StoreHandle> {
        match loc {
            StoreLocation::LocalFile(path) => {
                let pool = Self::open_sqlite_migrated(path.clone(), migrations).await?;
                Ok(StoreHandle::Sqlite(pool))
            }
            StoreLocation::SharedSchema(_) | StoreLocation::PathDerivedSchema(_) => {
                Err(anyhow::anyhow!(
                    "LocalFileBackend only opens LocalFile locations; use PostgresBackend for schema locations"
                ))
            }
        }
    }
}

/// Apply versioned migrations to a SQLite pool, idempotently, recording applied
/// versions in a `schema_migrations` table (mirrors the Postgres migrator).
async fn run_sqlite_migrations(pool: &SqlitePool, migrations: &[Migration]) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version     INTEGER PRIMARY KEY,
             description TEXT NOT NULL,
             applied_at  TEXT NOT NULL DEFAULT (datetime('now'))
         )",
    )
    .execute(pool)
    .await?;

    sqlx::query("BEGIN IMMEDIATE").execute(pool).await?;
    let result: anyhow::Result<()> = async {
        let applied_versions: HashSet<i64> =
            sqlx::query_scalar("SELECT version FROM schema_migrations")
                .fetch_all(pool)
                .await?
                .into_iter()
                .collect();

        let mut pending: Vec<&Migration> = migrations
            .iter()
            .filter(|migration| !applied_versions.contains(&i64::from(migration.version)))
            .collect();
        pending.sort_by_key(|migration| migration.version);

        for migration in pending {
            let version = i64::from(migration.version);
            sqlx::raw_sql(migration.sql)
                .execute(pool)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "SQLite migration v{} '{}' failed: {}",
                        migration.version,
                        migration.description,
                        error
                    )
                })?;
            sqlx::query("INSERT INTO schema_migrations (version, description) VALUES (?, ?)")
                .bind(version)
                .bind(migration.description)
                .execute(pool)
                .await?;
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            sqlx::query("COMMIT").execute(pool).await?;
            Ok(())
        }
        Err(error) => {
            if let Err(rollback_error) = sqlx::query("ROLLBACK").execute(pool).await {
                return Err(anyhow::anyhow!(
                    "{}; additionally failed to roll back SQLite migration transaction: {}",
                    error,
                    rollback_error
                ));
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T_MIGRATIONS: &[Migration] = &[Migration {
        version: 1,
        description: "create t",
        sql: "CREATE TABLE t (id TEXT PRIMARY KEY, v TEXT NOT NULL)",
    }];

    const SEMICOLON_MIGRATIONS: &[Migration] = &[Migration {
        version: 1,
        description: "create notes with semicolon literal",
        sql: "CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT NOT NULL);
              INSERT INTO notes (id, body) VALUES ('a', 'hello;world')",
    }];

    #[test]
    fn store_location_variants_construct() {
        let _shared = StoreLocation::SharedSchema("workflow_runtime".to_string());
        let _legacy = StoreLocation::PathDerivedSchema(PathBuf::from("/tmp/x/threads"));
        let _local = StoreLocation::LocalFile(PathBuf::from("/tmp/ws/threads.sqlite"));
    }

    #[tokio::test]
    async fn postgres_backend_rejects_local_file() {
        let backend = PostgresBackend::new(None);
        let err = backend
            .open_migrated(
                &StoreLocation::LocalFile(PathBuf::from("/tmp/ws/x.sqlite")),
                &[],
            )
            .await
            .expect_err("PostgresBackend must reject LocalFile");
        assert!(err.to_string().contains("LocalFileBackend"));
    }

    #[tokio::test]
    async fn local_file_backend_rejects_schema_locations() {
        let backend = LocalFileBackend::new();
        let err = backend
            .open_migrated(&StoreLocation::SharedSchema("s".into()), &[])
            .await
            .expect_err("LocalFileBackend must reject schema locations");
        assert!(err.to_string().contains("PostgresBackend"));
    }

    #[tokio::test]
    async fn local_file_backend_opens_migrates_and_persists() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Nested path to also exercise parent-dir creation.
        let path = dir.path().join("workspaces/job-1/threads.sqlite");
        let backend = LocalFileBackend::new();

        let handle = backend
            .open_migrated(&StoreLocation::LocalFile(path.clone()), T_MIGRATIONS)
            .await
            .expect("open sqlite");
        let pool = handle.as_sqlite().expect("sqlite handle");
        sqlx::query("INSERT INTO t (id, v) VALUES (?, ?)")
            .bind("a")
            .bind("1")
            .execute(pool)
            .await
            .expect("insert");
        drop(handle);

        // Reopen the same file: migrations are idempotent and data persists.
        let handle2 = backend
            .open_migrated(&StoreLocation::LocalFile(path.clone()), T_MIGRATIONS)
            .await
            .expect("reopen sqlite");
        let pool2 = handle2.as_sqlite().expect("sqlite handle");
        let (v,): (String,) = sqlx::query_as("SELECT v FROM t WHERE id = ?")
            .bind("a")
            .fetch_one(pool2)
            .await
            .expect("read back");
        assert_eq!(v, "1");
        assert!(path.exists(), "sqlite file should exist on disk");
    }

    #[tokio::test]
    async fn local_file_backend_runs_full_sqlite_migration_scripts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.sqlite");

        let pool = LocalFileBackend::open_sqlite_migrated(path.clone(), SEMICOLON_MIGRATIONS)
            .await
            .expect("open sqlite");
        let (body,): (String,) = sqlx::query_as("SELECT body FROM notes WHERE id = ?")
            .bind("a")
            .fetch_one(&pool)
            .await
            .expect("read note");
        assert_eq!(body, "hello;world");
    }

    #[tokio::test]
    async fn local_file_backend_serializes_concurrent_migrations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared.sqlite");
        let path_a = path.clone();
        let path_b = path.clone();

        let (pool_a, pool_b) = tokio::join!(
            LocalFileBackend::open_sqlite_migrated(path_a, T_MIGRATIONS),
            LocalFileBackend::open_sqlite_migrated(path_b, T_MIGRATIONS)
        );
        let pool_a = pool_a.expect("first concurrent open");
        let _pool_b = pool_b.expect("second concurrent open");

        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM schema_migrations WHERE version = 1")
                .fetch_one(&pool_a)
                .await
                .expect("count migrations");
        assert_eq!(count, 1);
    }
}
