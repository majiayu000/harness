mod migrations;
mod queries_aux;
mod queries_metrics;
mod queries_recovery;
mod queries_tasks;
mod raw_column_test_overrides;
mod types;

pub use types::{RecoveryResult, TaskArtifact, TaskCheckpoint, TaskPrompt};

use harness_core::db::{PgMigrator, PgStoreContext};
use harness_core::store_backend::{PostgresBackend, StoreLocation};
use migrations::TASK_MIGRATIONS;
use sqlx::postgres::PgPool;
use std::path::Path;

pub const TASK_DB_SCHEMA: &str = "task_db";

pub struct TaskDb {
    pool: PgPool,
    schema: String,
    store_key: String,
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
        let context = PgStoreContext::from_legacy_path_schema(db_path, configured_database_url)?;
        let schema = context.schema().to_owned();
        let store_key = schema.clone();
        let pool = context.open_migrated_pool(TASK_MIGRATIONS).await?;
        Ok(Self {
            pool,
            schema,
            store_key,
        })
    }

    pub fn shared_schema_context(
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<PgStoreContext> {
        PostgresBackend::new(configured_database_url.map(ToOwned::to_owned))
            .store_context(&StoreLocation::SharedSchema(TASK_DB_SCHEMA.to_string()))
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

    async fn open_with_context_and_store_key(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        store_key: String,
    ) -> anyhow::Result<Self> {
        let schema = context.schema().to_owned();
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, TASK_MIGRATIONS)
            .await?;
        Ok(Self {
            pool,
            schema,
            store_key,
        })
    }

    pub async fn from_pg_pool(pool: PgPool) -> anyhow::Result<Self> {
        let schema: String = sqlx::query_scalar("SELECT current_schema()")
            .fetch_one(&pool)
            .await?;
        let db = Self {
            pool,
            store_key: schema.clone(),
            schema,
        };
        PgMigrator::new(&db.pool, TASK_MIGRATIONS).run().await?;
        Ok(db)
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn store_key_for_data_dir(data_dir: &Path) -> anyhow::Result<String> {
        let canonical = data_dir.canonicalize().map_err(|error| {
            anyhow::anyhow!(
                "failed to canonicalize task data_dir {}: {error}",
                data_dir.display()
            )
        })?;
        Ok(canonical.to_string_lossy().into_owned())
    }

    pub fn store_key(&self) -> &str {
        &self.store_key
    }

    /// Generate Postgres-style positional placeholders: `$start, $start+1, ..., $start+count-1`.
    pub(super) fn numbered_placeholders(start: usize, count: usize) -> String {
        (start..start + count)
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub async fn migrate_legacy_task_db_if_needed(
    legacy_path: &Path,
    configured_database_url: Option<&str>,
    target_db: &TaskDb,
) -> anyhow::Result<u64> {
    let legacy_context =
        PgStoreContext::from_legacy_path_schema(legacy_path, configured_database_url)?;
    let legacy_schema = legacy_context.schema();
    if legacy_schema == target_db.schema() {
        return Ok(0);
    }

    let already_backfilled: Option<String> = sqlx::query_scalar(
        "SELECT legacy_schema
         FROM task_db_legacy_backfills
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
        .bind(format!("{}.tasks", quote_pg_ident(legacy_schema)))
        .fetch_one(&target_db.pool)
        .await?;
    if legacy_table.is_none() {
        return Ok(0);
    }

    let legacy_pool = legacy_context.open_migrated_pool(TASK_MIGRATIONS).await?;
    legacy_pool.close().await;

    let legacy_schema_sql = quote_pg_ident(legacy_schema);
    let mut tx = target_db.pool.begin().await?;

    let tasks_sql = format!(
        "INSERT INTO tasks (
            store_key, id, task_kind, status, failure_kind, turn, pr_url, rounds, error, source,
            external_id, parent_id, created_at, updated_at, repo, depends_on, project,
            workspace_path, workspace_owner, run_generation, priority, phase, description,
            request_settings, scheduler_state, version, system_input
         )
         SELECT $1, id, task_kind, status, failure_kind, turn, pr_url, rounds, error, source,
                external_id, parent_id, created_at, updated_at, repo, depends_on, project,
                workspace_path, workspace_owner, run_generation, priority, phase, description,
                request_settings, scheduler_state, version, system_input
         FROM {legacy_schema_sql}.tasks
         ON CONFLICT (store_key, id) DO NOTHING"
    );
    let copied_tasks = sqlx::query(&tasks_sql)
        .bind(target_db.store_key())
        .execute(&mut *tx)
        .await?
        .rows_affected();

    let artifacts_sql = format!(
        "INSERT INTO task_artifacts (store_key, task_id, turn, artifact_type, content, created_at)
         SELECT $1, task_id, turn, artifact_type, content, created_at
         FROM {legacy_schema_sql}.task_artifacts
         ORDER BY id
         ON CONFLICT DO NOTHING"
    );
    sqlx::query(&artifacts_sql)
        .bind(target_db.store_key())
        .execute(&mut *tx)
        .await?;

    let checkpoints_sql = format!(
        "INSERT INTO task_checkpoints (
            store_key, task_id, triage_output, plan_output, pr_url, last_phase, updated_at
         )
         SELECT $1, task_id, triage_output, plan_output, pr_url, last_phase, updated_at
         FROM {legacy_schema_sql}.task_checkpoints
         ON CONFLICT (store_key, task_id) DO NOTHING"
    );
    sqlx::query(&checkpoints_sql)
        .bind(target_db.store_key())
        .execute(&mut *tx)
        .await?;

    let prompts_sql = format!(
        "INSERT INTO task_prompts (store_key, task_id, turn, phase, prompt, created_at)
         SELECT $1, task_id, turn, phase, prompt, created_at
         FROM {legacy_schema_sql}.task_prompts
         ORDER BY id
         ON CONFLICT (store_key, task_id, turn, phase) DO NOTHING"
    );
    sqlx::query(&prompts_sql)
        .bind(target_db.store_key())
        .execute(&mut *tx)
        .await?;

    let leases_sql = format!(
        "INSERT INTO workspace_leases (
            store_key, project_key, slot_index, task_id, workspace_key, workspace_path, source_repo,
            repo, runtime_workflow_id, owner_session, run_generation, process_id,
            process_started_at, state, acquired_at, released_at, last_used_at
         )
         SELECT $1, project_key, slot_index, task_id, workspace_key, workspace_path, source_repo,
                repo, runtime_workflow_id, owner_session, run_generation, process_id,
                process_started_at, state, acquired_at, released_at, last_used_at
         FROM {legacy_schema_sql}.workspace_leases
         ON CONFLICT (store_key, project_key, slot_index) DO NOTHING"
    );
    sqlx::query(&leases_sql)
        .bind(target_db.store_key())
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "INSERT INTO task_db_legacy_backfills (store_key, legacy_schema, copied_rows)
         VALUES ($1, $2, $3)
         ON CONFLICT (store_key, legacy_schema) DO NOTHING",
    )
    .bind(target_db.store_key())
    .bind(legacy_schema)
    .bind(copied_tasks as i64)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    if copied_tasks > 0 {
        tracing::info!(
            copied = copied_tasks,
            legacy_schema,
            target_schema = target_db.schema(),
            "task db migration: backfilled legacy tasks into shared schema"
        );
    }

    Ok(copied_tasks)
}

fn quote_pg_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
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
