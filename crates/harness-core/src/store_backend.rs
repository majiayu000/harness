//! Storage backend seam.
//!
//! Decouples *where a store lives* (`StoreLocation`) from *how its pool is
//! opened*, so the rest of the system can construct stores through a single
//! `Backend` trait instead of calling `PgStoreContext` / `pg_schema_for_path`
//! directly. This is Phase 2 of the storage-layer redesign (see
//! `docs/rfc-storage-layer-redesign.md`).
//!
//! Phase 2 scope (this module): a Postgres-only backend that wraps the existing
//! `PgStoreContext` with **zero behavior change**. It also makes the two schema
//! strategies explicit:
//!   - [`StoreLocation::SharedSchema`] — a fixed, bounded shared schema (the
//!     target for durable orchestration state).
//!   - [`StoreLocation::PathDerivedSchema`] — the legacy "one schema per
//!     store-identity path" strategy. This is the coupling that caused the
//!     ~539k-schema explosion and is slated for removal once callers move to
//!     shared schemas / the local-file backend.
//!
//! Phase 3 will add a `LocalFile` backend for ephemeral per-workspace scratch;
//! its handle type differs from `PgPool`, so the trait's return type will be
//! generalized then. Today every store holds a `PgPool`, so returning one keeps
//! the seam honest and avoids premature abstraction.

use std::path::PathBuf;

use async_trait::async_trait;
use sqlx::postgres::PgPool;

use crate::db::{Migration, PgStoreContext};

/// Logical location of a store, independent of the physical backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StoreLocation {
    /// Durable, shared: a fixed, named Postgres schema (bounded count).
    SharedSchema(String),
    /// Legacy: schema derived by hashing a store-identity path
    /// (`pg_schema_for_path`). Produces one schema per distinct path — the
    /// source of the schema explosion. Retained only until callers migrate to
    /// [`StoreLocation::SharedSchema`] or the local-file backend.
    PathDerivedSchema(PathBuf),
    /// Ephemeral, local file under a workspace directory (Phase 3 — not wired).
    LocalFile(PathBuf),
}

/// Opens (and migrates) a store for a given [`StoreLocation`].
#[async_trait]
pub trait Backend: Send + Sync {
    /// Open a migrated connection pool for the store at `loc`.
    async fn open_migrated(
        &self,
        loc: &StoreLocation,
        migrations: &[Migration],
    ) -> anyhow::Result<PgPool>;
}

/// Postgres-backed implementation that wraps the existing `PgStoreContext`.
///
/// Behavior is identical to the current direct `PgStoreContext` usage; this type
/// only centralizes the construction so the location strategy lives in one place.
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
}

#[async_trait]
impl Backend for PostgresBackend {
    async fn open_migrated(
        &self,
        loc: &StoreLocation,
        migrations: &[Migration],
    ) -> anyhow::Result<PgPool> {
        let url = self.configured_database_url.as_deref();
        let context = match loc {
            StoreLocation::SharedSchema(schema) => PgStoreContext::from_schema(schema, url)?,
            StoreLocation::PathDerivedSchema(path) => PgStoreContext::from_path(path, url)?,
            StoreLocation::LocalFile(path) => anyhow::bail!(
                "LocalFile backend is not implemented yet (RFC storage redesign Phase 3): {}",
                path.display()
            ),
        };
        context.open_migrated_pool(migrations).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_location_variants_construct() {
        let _shared = StoreLocation::SharedSchema("workflow_runtime".to_string());
        let _legacy = StoreLocation::PathDerivedSchema(PathBuf::from("/tmp/x/threads"));
        let _local = StoreLocation::LocalFile(PathBuf::from("/tmp/ws/threads.sqlite"));
    }

    #[tokio::test]
    async fn local_file_backend_is_not_yet_supported() {
        let backend = PostgresBackend::new(None);
        let err = backend
            .open_migrated(
                &StoreLocation::LocalFile(PathBuf::from("/tmp/ws/x.sqlite")),
                &[],
            )
            .await
            .expect_err("LocalFile must error until Phase 3");
        assert!(err.to_string().contains("Phase 3"));
    }
}
