mod migrations;
mod queries_aux;
mod queries_tasks;
mod types;

pub use types::{RecoveryResult, TaskArtifact, TaskCheckpoint, TaskPrompt};

use harness_core::db::{
    pg_create_schema_if_not_exists, pg_open_pool, pg_open_pool_schematized, resolve_database_url,
    PgMigrator,
};
use migrations::TASK_MIGRATIONS;
use sqlx::postgres::PgPool;
use std::path::Path;

pub struct TaskDb {
    pool: PgPool,
}

impl TaskDb {
    /// Open a Postgres-backed task database.
    ///
    /// Each unique `db_path` maps to its own Postgres schema (`h{hash_of_path}`)
    /// so that multiple instances — including parallel integration tests — remain
    /// fully isolated. The schema is created if it does not exist and migrations
    /// are applied before any queries run.
    pub async fn open(db_path: &Path) -> anyhow::Result<Self> {
        Self::open_with_database_url(db_path, None).await
    }

    pub async fn open_with_database_url(
        db_path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let database_url = resolve_database_url(configured_database_url)?;
        use sha2::{Digest, Sha256};
        let path_utf8 = db_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {:?}", db_path))?;
        let digest = Sha256::digest(path_utf8.as_bytes());
        let mut schema_bytes = [0u8; 8];
        schema_bytes.copy_from_slice(&digest[..8]);
        let schema = format!("h{:016x}", u64::from_le_bytes(schema_bytes));

        let setup = pg_open_pool(&database_url).await?;
        pg_create_schema_if_not_exists(&setup, &schema).await?;
        setup.close().await;

        let pool = pg_open_pool_schematized(&database_url, &schema).await?;
        Self::from_pg_pool(pool).await
    }

    pub async fn from_pg_pool(pool: PgPool) -> anyhow::Result<Self> {
        let db = Self { pool };
        PgMigrator::new(&db.pool, TASK_MIGRATIONS).run().await?;
        Ok(db)
    }

    /// Generate Postgres-style positional placeholders: `$start, $start+1, ..., $start+count-1`.
    pub(super) fn numbered_placeholders(start: usize, count: usize) -> String {
        (start..start + count)
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_placeholders_single() {
        assert_eq!(TaskDb::numbered_placeholders(1, 1), "$1");
    }

    #[test]
    fn numbered_placeholders_range() {
        assert_eq!(TaskDb::numbered_placeholders(1, 3), "$1, $2, $3");
    }

    #[test]
    fn numbered_placeholders_offset() {
        assert_eq!(TaskDb::numbered_placeholders(4, 2), "$4, $5");
    }
}
