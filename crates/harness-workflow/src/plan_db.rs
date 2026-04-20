use harness_core::db::{
    pg_create_schema_if_not_exists, pg_open_pool, pg_open_pool_schematized, Migration, PgMigrator,
};
use harness_core::{types::ExecPlanId, types::ExecPlanStatus};
use harness_exec::plan::ExecPlan;
use sqlx::postgres::PgPool;
use std::path::Path;

static PLAN_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create exec_plans table",
        sql: "CREATE TABLE IF NOT EXISTS exec_plans (
            id         TEXT PRIMARY KEY,
            data       TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 2,
        description: "add index on exec_plans(created_at)",
        sql: "CREATE INDEX IF NOT EXISTS idx_exec_plans_created_at ON exec_plans(created_at)",
    },
    Migration {
        version: 3,
        description: "convert exec_plans.data from TEXT to JSONB",
        sql: "ALTER TABLE exec_plans ALTER COLUMN data TYPE JSONB USING replace(data, '\\u0000', '')::jsonb",
    },
    Migration {
        version: 4,
        description: "add GIN index on exec_plans.data for JSONB path queries",
        sql: "CREATE INDEX IF NOT EXISTS idx_exec_plans_data_gin ON exec_plans USING GIN (data jsonb_path_ops)",
    },
];

pub struct PlanDb {
    pool: PgPool,
    /// Serializes read-modify-write cycles to prevent lost-update races.
    update_lock: tokio::sync::Mutex<()>,
}

impl PlanDb {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| anyhow::anyhow!("DATABASE_URL environment variable is not set"))?;
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
        let mut schema_bytes = [0u8; 8];
        schema_bytes.copy_from_slice(&digest[..8]);
        let schema = format!("h{:016x}", u64::from_le_bytes(schema_bytes));

        let setup = pg_open_pool(&database_url).await?;
        pg_create_schema_if_not_exists(&setup, &schema).await?;
        setup.close().await;

        let pool = pg_open_pool_schematized(&database_url, &schema).await?;
        PgMigrator::new(&pool, PLAN_MIGRATIONS).run().await?;
        Ok(Self {
            pool,
            update_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub async fn upsert(&self, plan: &ExecPlan) -> anyhow::Result<()> {
        let data = serde_json::to_string(plan)?;
        // PostgreSQL JSONB rejects \u0000; serde_json encodes NUL as the
        // 6-char escape sequence, so strip that form (not the literal byte).
        let data = if data.contains("\\u0000") {
            data.replace("\\u0000", "")
        } else {
            data
        };
        sqlx::query(
            "INSERT INTO exec_plans (id, data) VALUES ($1, $2::jsonb)
             ON CONFLICT(id) DO UPDATE SET data = EXCLUDED.data,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(plan.id.as_str())
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load a plan, apply `f`, persist the result, and return the updated plan.
    ///
    /// Holds an exclusive lock for the duration of the read-modify-write so
    /// that concurrent callers cannot overwrite each other's changes.
    pub async fn update_in_txn<F>(&self, id: &ExecPlanId, f: F) -> anyhow::Result<Option<ExecPlan>>
    where
        F: FnOnce(&mut ExecPlan),
    {
        let _guard = self.update_lock.lock().await;
        let Some(mut plan) = self.get(id).await? else {
            return Ok(None);
        };
        f(&mut plan);
        self.upsert(&plan).await?;
        Ok(Some(plan))
    }

    pub async fn get(&self, id: &ExecPlanId) -> anyhow::Result<Option<ExecPlan>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM exec_plans WHERE id = $1")
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await?;
        match row {
            Some((data,)) => Ok(Some(serde_json::from_str(&data)?)),
            None => Ok(None),
        }
    }

    pub async fn list(&self) -> anyhow::Result<Vec<ExecPlan>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT data::text FROM exec_plans ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn delete(&self, id: &ExecPlanId) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM exec_plans WHERE id = $1")
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Return all plans whose status matches `status`.
    pub async fn list_by_status(&self, status: ExecPlanStatus) -> anyhow::Result<Vec<ExecPlan>> {
        let filter = serde_json::json!({ "status": status });
        let filter_str = serde_json::to_string(&filter)?;
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM exec_plans WHERE data @> $1::jsonb ORDER BY created_at DESC",
        )
        .bind(&filter_str)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    /// Return all plans whose purpose contains `query` (case-insensitive substring match).
    pub async fn search_by_name(&self, query: &str) -> anyhow::Result<Vec<ExecPlan>> {
        let pattern = format!("%{}%", query);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM exec_plans WHERE data->>'purpose' LIKE $1 ORDER BY created_at DESC",
        )
        .bind(&pattern)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    /// Import ExecPlan Markdown files from `dir` into the database.
    ///
    /// Files are only imported when no plan with the same ID already exists,
    /// so re-running migration on subsequent startups is safe (idempotent).
    pub async fn migrate_from_markdown_dir(&self, dir: &Path) -> anyhow::Result<usize> {
        if !dir.is_dir() {
            return Ok(0);
        }
        let mut imported = 0usize;
        let entries = std::fs::read_dir(dir)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "plan migration: failed to read file: {e}");
                    continue;
                }
            };
            let plan = match ExecPlan::from_markdown(&content) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "plan migration: failed to parse markdown: {e}");
                    continue;
                }
            };
            if self.get(&plan.id).await?.is_some() {
                continue;
            }
            if let Err(e) = self.upsert(&plan).await {
                tracing::warn!(path = %path.display(), "plan migration: failed to upsert: {e}");
                continue;
            }
            tracing::info!(id = plan.id.as_str(), purpose = %plan.purpose, "plan migration: imported from markdown");
            imported += 1;
        }
        Ok(imported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::types::ExecPlanStatus;
    use std::sync::OnceLock;
    use tokio::sync::{Semaphore, SemaphorePermit};

    // Serialize plan_db tests to stay within the Supabase session-mode
    // connection limit (15). Each pool uses up to 8 connections; concurrent
    // tests would exhaust the limit. One test at a time uses ≤8 connections.
    static DB_GATE: OnceLock<Semaphore> = OnceLock::new();
    fn db_gate() -> &'static Semaphore {
        DB_GATE.get_or_init(|| Semaphore::new(1))
    }

    async fn open_test_store(
    ) -> anyhow::Result<Option<(PlanDb, tempfile::TempDir, SemaphorePermit<'static>)>> {
        if std::env::var("DATABASE_URL").is_err() {
            return Ok(None);
        }
        let permit = db_gate().acquire().await?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("plans.db");
        let db = PlanDb::open(&path).await?;
        Ok(Some((db, dir, permit)))
    }

    #[tokio::test]
    async fn plan_db_roundtrip() -> anyhow::Result<()> {
        let Some((db, dir, _permit)) = open_test_store().await? else {
            return Ok(());
        };
        let plan = ExecPlan::from_spec("Test plan purpose", dir.path())?;
        db.upsert(&plan).await?;

        let loaded = db.get(&plan.id).await?.expect("plan should exist");
        assert_eq!(loaded.id.as_str(), plan.id.as_str());
        assert_eq!(loaded.purpose, plan.purpose);

        let all = db.list().await?;
        assert_eq!(all.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn plan_db_get_missing_returns_none() -> anyhow::Result<()> {
        let Some((db, _dir, _permit)) = open_test_store().await? else {
            return Ok(());
        };
        assert!(db.get(&ExecPlanId::new()).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn plan_db_delete_removes_plan() -> anyhow::Result<()> {
        let Some((db, dir, _permit)) = open_test_store().await? else {
            return Ok(());
        };
        let plan = ExecPlan::from_spec("# Delete me", dir.path())?;
        db.upsert(&plan).await?;
        assert!(db.delete(&plan.id).await?);
        assert!(db.get(&plan.id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn plan_db_delete_missing_returns_false() -> anyhow::Result<()> {
        let Some((db, _dir, _permit)) = open_test_store().await? else {
            return Ok(());
        };
        assert!(!db.delete(&ExecPlanId::new()).await?);
        Ok(())
    }

    #[tokio::test]
    async fn list_by_status_filters_correctly() -> anyhow::Result<()> {
        let Some((db, dir, _permit)) = open_test_store().await? else {
            return Ok(());
        };
        let draft = ExecPlan::from_spec("# Draft plan", dir.path())?;
        let mut active = ExecPlan::from_spec("# Active plan", dir.path())?;
        active.activate();
        let mut completed = ExecPlan::from_spec("# Completed plan", dir.path())?;
        completed.complete();

        db.upsert(&draft).await?;
        db.upsert(&active).await?;
        db.upsert(&completed).await?;

        let drafts = db.list_by_status(ExecPlanStatus::Draft).await?;
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].id.as_str(), draft.id.as_str());

        let actives = db.list_by_status(ExecPlanStatus::Active).await?;
        assert_eq!(actives.len(), 1);
        assert_eq!(actives[0].id.as_str(), active.id.as_str());
        Ok(())
    }

    #[tokio::test]
    async fn search_by_name_finds_matching_plans() -> anyhow::Result<()> {
        let Some((db, dir, _permit)) = open_test_store().await? else {
            return Ok(());
        };
        let auth = ExecPlan::from_spec("# Implement authentication", dir.path())?;
        let deploy = ExecPlan::from_spec("# Deploy to production", dir.path())?;

        db.upsert(&auth).await?;
        db.upsert(&deploy).await?;

        let results = db.search_by_name("auth").await?;
        assert_eq!(results.len(), 1);
        assert!(results[0].purpose.to_lowercase().contains("auth"));

        let all = db.search_by_name("").await?;
        assert_eq!(all.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn migrate_from_markdown_dir_imports_plans() -> anyhow::Result<()> {
        let Some((db, dir, _permit)) = open_test_store().await? else {
            return Ok(());
        };
        let plan = ExecPlan::from_spec("# Migration test plan", dir.path())?;
        let md = plan.to_markdown();
        std::fs::write(dir.path().join("plan1.md"), &md)?;

        let count = db.migrate_from_markdown_dir(dir.path()).await?;
        assert_eq!(count, 1);

        let loaded = db.get(&plan.id).await?.expect("migrated plan should exist");
        assert_eq!(loaded.purpose, plan.purpose);

        let count2 = db.migrate_from_markdown_dir(dir.path()).await?;
        assert_eq!(count2, 0);
        Ok(())
    }

    #[tokio::test]
    async fn migrate_from_markdown_dir_skips_nonexistent_dir() -> anyhow::Result<()> {
        let Some((db, dir, _permit)) = open_test_store().await? else {
            return Ok(());
        };
        let count = db
            .migrate_from_markdown_dir(&dir.path().join("nonexistent"))
            .await?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn update_in_txn_persists_mutation() -> anyhow::Result<()> {
        let Some((db, dir, _permit)) = open_test_store().await? else {
            return Ok(());
        };
        let plan = ExecPlan::from_spec("# Update test", dir.path())?;
        db.upsert(&plan).await?;

        let updated = db
            .update_in_txn(&plan.id, |p| p.activate())
            .await?
            .ok_or_else(|| anyhow::anyhow!("plan should exist after upsert"))?;
        assert_eq!(updated.status, ExecPlanStatus::Active);

        let reloaded = db
            .get(&plan.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("plan should still exist after update"))?;
        assert_eq!(reloaded.status, ExecPlanStatus::Active);
        Ok(())
    }

    #[tokio::test]
    async fn update_in_txn_returns_none_for_missing_plan() -> anyhow::Result<()> {
        let Some((db, _dir, _permit)) = open_test_store().await? else {
            return Ok(());
        };
        let result: Option<ExecPlan> = db
            .update_in_txn(&ExecPlanId::new(), |p| p.activate())
            .await?;
        assert!(result.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn upsert_sanitizes_nul_bytes() -> anyhow::Result<()> {
        let Some((db, dir, _permit)) = open_test_store().await? else {
            return Ok(());
        };
        // PostgreSQL JSONB rejects \u0000; a spec with an embedded NUL must not fail.
        let plan = ExecPlan::from_spec("# a\0b", dir.path())?;
        db.upsert(&plan).await?;
        let loaded = db
            .get(&plan.id)
            .await?
            .expect("plan should exist after nul-stripped upsert");
        assert!(
            loaded.purpose.contains('a'),
            "purpose should contain 'a' after NUL stripping"
        );
        Ok(())
    }

    /// Verifies the upgrade contract: a database with v1+v2 migrations (data TEXT)
    /// is correctly upgraded to v3 (JSONB column) and v4 (GIN index), and that
    /// pre-existing TEXT rows survive the `USING data::jsonb` cast.
    #[tokio::test]
    async fn migration_contract_text_to_jsonb_gin() -> anyhow::Result<()> {
        let Ok(database_url) = std::env::var("DATABASE_URL") else {
            return Ok(());
        };
        let _permit = db_gate().acquire().await?;

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("migrate_contract");
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
        let mut schema_bytes = [0u8; 8];
        schema_bytes.copy_from_slice(&digest[..8]);
        let schema = format!("h{:016x}", u64::from_le_bytes(schema_bytes));

        let setup = pg_open_pool(&database_url).await?;
        sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", schema))
            .execute(&setup)
            .await?;
        drop(setup);

        let pool = pg_open_pool_schematized(&database_url, &schema).await?;

        // Simulate a pre-existing v1+v2 database where data is TEXT
        PgMigrator::new(&pool, &PLAN_MIGRATIONS[..2]).run().await?;

        // Insert a row with valid JSON stored as TEXT
        let plan_id = ExecPlanId::new();
        sqlx::query("INSERT INTO exec_plans (id, data) VALUES ($1, $2)")
            .bind(plan_id.as_str())
            .bind(r#"{"id":"test","status":"draft","purpose":"migration contract"}"#)
            .execute(&pool)
            .await?;

        // Apply the remaining migrations (v3: TEXT→JSONB, v4: GIN index)
        PgMigrator::new(&pool, PLAN_MIGRATIONS).run().await?;

        // Assert: data column is now jsonb
        let (col_type,): (String,) = sqlx::query_as(
            "SELECT data_type FROM information_schema.columns
             WHERE table_schema = current_schema()
               AND table_name   = 'exec_plans'
               AND column_name  = 'data'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            col_type, "jsonb",
            "data column must be jsonb after migration"
        );

        // Assert: GIN index was created
        let idx: Option<(String,)> = sqlx::query_as(
            "SELECT indexname::text FROM pg_indexes
             WHERE schemaname = current_schema()
               AND tablename  = 'exec_plans'
               AND indexname  = 'idx_exec_plans_data_gin'",
        )
        .fetch_optional(&pool)
        .await?;
        assert!(
            idx.is_some(),
            "idx_exec_plans_data_gin must exist after migration"
        );

        // Assert: pre-existing TEXT row survived the JSONB conversion
        let (data,): (String,) = sqlx::query_as("SELECT data::text FROM exec_plans WHERE id = $1")
            .bind(plan_id.as_str())
            .fetch_one(&pool)
            .await?;
        let parsed: serde_json::Value = serde_json::from_str(&data)?;
        assert_eq!(parsed["status"], serde_json::json!("draft"));

        Ok(())
    }
}
