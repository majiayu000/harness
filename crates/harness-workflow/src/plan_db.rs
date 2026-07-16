use anyhow::Context;
use harness_core::db::{Migration, PgStoreContext};
use harness_core::store_backend::{PostgresBackend, StoreLocation};
use harness_core::{types::ExecPlanId, types::ExecPlanStatus};
use harness_exec::plan::ExecPlan;
use sqlx::postgres::PgPool;
use std::path::Path;

pub const PLAN_DB_SCHEMA: &str = "plan_db";

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
    Migration {
        version: 5,
        description: "scope exec plans within shared schema",
        sql: "ALTER TABLE exec_plans ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE exec_plans
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE exec_plans ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE exec_plans ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE exec_plans DROP CONSTRAINT IF EXISTS exec_plans_pkey;
              ALTER TABLE exec_plans ADD CONSTRAINT exec_plans_pkey PRIMARY KEY (store_key, id);
              CREATE INDEX IF NOT EXISTS idx_exec_plans_store_created_at
              ON exec_plans(store_key, created_at DESC)",
    },
    Migration {
        version: 6,
        description: "record legacy plan db backfills",
        sql: "CREATE TABLE IF NOT EXISTS plan_db_legacy_backfills (
            store_key     TEXT NOT NULL DEFAULT current_schema(),
            legacy_schema TEXT NOT NULL,
            copied_rows   BIGINT NOT NULL DEFAULT 0,
            backfilled_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (store_key, legacy_schema)
        )",
    },
];

pub struct PlanDb {
    pool: PgPool,
    schema: String,
    store_key: String,
    /// Serializes read-modify-write cycles to prevent lost-update races.
    update_lock: tokio::sync::Mutex<()>,
}

impl PlanDb {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_database_url(path, None).await
    }

    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let context = PgStoreContext::from_legacy_path_schema(path, configured_database_url)?;
        let schema = context.schema().to_owned();
        let store_key = schema.clone();
        let pool = context.open_migrated_pool(PLAN_MIGRATIONS).await?;
        Ok(Self {
            pool,
            schema,
            store_key,
            update_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn shared_schema_context(
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<PgStoreContext> {
        PostgresBackend::new(configured_database_url.map(ToOwned::to_owned))
            .store_context(&StoreLocation::SharedSchema(PLAN_DB_SCHEMA.to_string()))
    }

    pub async fn open_with_context(
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        Self::open_with_context_and_store_key(context, setup_pool, context.schema().to_owned())
            .await
    }

    pub async fn open_shared_with_data_dir(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        data_dir: &Path,
    ) -> anyhow::Result<Self> {
        let store_key = Self::store_key_for_data_dir(data_dir)?;
        Self::open_with_context_and_store_key(context, setup_pool, store_key).await
    }

    pub async fn open_shared_with_legacy_backfill(
        configured_database_url: Option<&str>,
        setup_pool: &PgPool,
        data_dir: &Path,
        legacy_path: &Path,
    ) -> anyhow::Result<Self> {
        let context = Self::shared_schema_context(configured_database_url)?;
        let db = Self::open_shared_with_data_dir(&context, setup_pool, data_dir).await?;
        migrate_legacy_plan_db_if_needed(legacy_path, configured_database_url, &db).await?;
        Ok(db)
    }

    async fn open_with_context_and_store_key(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        store_key: String,
    ) -> anyhow::Result<Self> {
        let schema = context.schema().to_owned();
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, PLAN_MIGRATIONS)
            .await?;
        Ok(Self {
            pool,
            schema,
            store_key,
            update_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn store_key_for_data_dir(data_dir: &Path) -> anyhow::Result<String> {
        std::fs::create_dir_all(data_dir).with_context(|| {
            format!("failed to create plan db data dir '{}'", data_dir.display())
        })?;
        let canonical = data_dir.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize plan db data dir '{}'",
                data_dir.display()
            )
        })?;
        Ok(canonical.to_string_lossy().into_owned())
    }

    pub fn store_key(&self) -> &str {
        &self.store_key
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
            "INSERT INTO exec_plans (store_key, id, data) VALUES ($1, $2, $3::jsonb)
             ON CONFLICT(store_key, id) DO UPDATE SET data = EXCLUDED.data,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&self.store_key)
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
            sqlx::query_as("SELECT data::text FROM exec_plans WHERE store_key = $1 AND id = $2")
                .bind(&self.store_key)
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await?;
        match row {
            Some((data,)) => Ok(Some(serde_json::from_str(&data)?)),
            None => Ok(None),
        }
    }

    pub async fn list(&self) -> anyhow::Result<Vec<ExecPlan>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM exec_plans WHERE store_key = $1 ORDER BY created_at DESC",
        )
        .bind(&self.store_key)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn delete(&self, id: &ExecPlanId) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM exec_plans WHERE store_key = $1 AND id = $2")
            .bind(&self.store_key)
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
            "SELECT data::text FROM exec_plans
             WHERE store_key = $1 AND data @> $2::jsonb
             ORDER BY created_at DESC",
        )
        .bind(&self.store_key)
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
            "SELECT data::text FROM exec_plans
             WHERE store_key = $1 AND data->>'purpose' LIKE $2
             ORDER BY created_at DESC",
        )
        .bind(&self.store_key)
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

pub async fn migrate_legacy_plan_db_if_needed(
    legacy_path: &Path,
    configured_database_url: Option<&str>,
    target_db: &PlanDb,
) -> anyhow::Result<u64> {
    let legacy_context =
        PgStoreContext::from_legacy_path_schema(legacy_path, configured_database_url)?;
    let legacy_schema = legacy_context.schema();
    if legacy_schema == target_db.schema() {
        return Ok(0);
    }

    let already_backfilled: Option<String> = sqlx::query_scalar(
        "SELECT legacy_schema
         FROM plan_db_legacy_backfills
         WHERE store_key = $1 AND legacy_schema = $2",
    )
    .bind(target_db.store_key())
    .bind(legacy_schema)
    .fetch_optional(&target_db.pool)
    .await?;
    if already_backfilled.is_some() {
        return Ok(0);
    }

    let legacy_table: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("\"{legacy_schema}\".exec_plans"))
        .fetch_one(&target_db.pool)
        .await?;
    if legacy_table.is_none() {
        return Ok(0);
    }

    let mut tx = target_db.pool.begin().await?;
    let copy_sql = format!(
        "INSERT INTO exec_plans (store_key, id, data, created_at, updated_at)
         SELECT $1, id, replace(data::text, '\\u0000', '')::jsonb, created_at, updated_at
         FROM \"{legacy_schema}\".exec_plans
         ON CONFLICT (store_key, id) DO NOTHING"
    );
    let copied = sqlx::query(&copy_sql)
        .bind(target_db.store_key())
        .execute(&mut *tx)
        .await?
        .rows_affected();

    sqlx::query(
        "INSERT INTO plan_db_legacy_backfills (store_key, legacy_schema, copied_rows)
         VALUES ($1, $2, $3)
         ON CONFLICT (store_key, legacy_schema) DO NOTHING",
    )
    .bind(target_db.store_key())
    .bind(legacy_schema)
    .bind(copied as i64)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    if copied > 0 {
        tracing::info!(
            copied,
            legacy_schema,
            target_schema = target_db.schema(),
            "plan db migration: backfilled legacy plans into shared schema"
        );
    }

    Ok(copied)
}

#[cfg(test)]
mod tests;
