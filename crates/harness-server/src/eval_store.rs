use chrono::{DateTime, Utc};
use harness_core::db::{Migration, PgStoreContext};
use harness_core::store_backend::{PostgresBackend, StoreLocation};
use harness_eval::{
    score_pr_repair_eval, EvalScenario, EvalTarget, PrRepairEvalInput, QualitySnapshot,
};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use std::path::Path;
use uuid::Uuid;

pub const EVAL_STORE_SCHEMA: &str = "eval_store";

static EVAL_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create eval runs table",
        sql: "CREATE TABLE IF NOT EXISTS eval_runs (
            id                  TEXT PRIMARY KEY,
            scenario            TEXT NOT NULL,
            target_kind         TEXT NOT NULL,
            target_repo         TEXT,
            target_pr_number    BIGINT,
            target_issue_number BIGINT,
            source_task_id      TEXT,
            status              TEXT NOT NULL,
            quality_snapshot_id TEXT,
            data                TEXT NOT NULL,
            created_at          TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at          TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            completed_at        TIMESTAMPTZ
        )",
    },
    Migration {
        version: 2,
        description: "create eval artifacts table",
        sql: "CREATE TABLE IF NOT EXISTS eval_artifacts (
            id            TEXT PRIMARY KEY,
            run_id        TEXT NOT NULL,
            artifact_type TEXT NOT NULL,
            label         TEXT,
            content_type  TEXT,
            body          TEXT NOT NULL,
            data          TEXT NOT NULL,
            created_at    TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (run_id) REFERENCES eval_runs(id) ON DELETE CASCADE
        )",
    },
    Migration {
        version: 3,
        description: "create quality snapshots table",
        sql: "CREATE TABLE IF NOT EXISTS quality_snapshots (
            id               TEXT PRIMARY KEY,
            run_id           TEXT NOT NULL,
            scenario         TEXT NOT NULL,
            target_repo      TEXT,
            target_pr_number BIGINT,
            final_score      BIGINT NOT NULL,
            final_grade      TEXT NOT NULL,
            data             TEXT NOT NULL,
            created_at       TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (run_id) REFERENCES eval_runs(id) ON DELETE CASCADE
        )",
    },
    Migration {
        version: 4,
        description: "create eval lookup indexes",
        sql: "CREATE INDEX IF NOT EXISTS idx_eval_runs_updated
              ON eval_runs(updated_at DESC);
              CREATE INDEX IF NOT EXISTS idx_eval_runs_pr
              ON eval_runs(target_repo, target_pr_number, updated_at DESC);
              CREATE INDEX IF NOT EXISTS idx_eval_artifacts_run
              ON eval_artifacts(run_id, created_at);
              CREATE INDEX IF NOT EXISTS idx_quality_snapshots_run
              ON quality_snapshots(run_id, created_at DESC);
              CREATE INDEX IF NOT EXISTS idx_quality_snapshots_pr
              ON quality_snapshots(target_repo, target_pr_number, created_at DESC)",
    },
    Migration {
        version: 5,
        description: "scope eval rows within shared schema",
        sql: "ALTER TABLE eval_runs ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE eval_runs
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE eval_runs ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE eval_runs ALTER COLUMN store_key SET NOT NULL;

              ALTER TABLE eval_artifacts ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE eval_artifacts
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE eval_artifacts ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE eval_artifacts ALTER COLUMN store_key SET NOT NULL;

              ALTER TABLE quality_snapshots ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE quality_snapshots
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE quality_snapshots ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE quality_snapshots ALTER COLUMN store_key SET NOT NULL;

              ALTER TABLE eval_artifacts DROP CONSTRAINT IF EXISTS eval_artifacts_run_id_fkey;
              ALTER TABLE eval_artifacts DROP CONSTRAINT IF EXISTS eval_artifacts_run_fk;
              ALTER TABLE quality_snapshots DROP CONSTRAINT IF EXISTS quality_snapshots_run_id_fkey;
              ALTER TABLE quality_snapshots DROP CONSTRAINT IF EXISTS quality_snapshots_run_fk;

              ALTER TABLE eval_runs DROP CONSTRAINT IF EXISTS eval_runs_pkey;
              ALTER TABLE eval_runs ADD CONSTRAINT eval_runs_pkey PRIMARY KEY (store_key, id);
              ALTER TABLE eval_artifacts DROP CONSTRAINT IF EXISTS eval_artifacts_pkey;
              ALTER TABLE eval_artifacts ADD CONSTRAINT eval_artifacts_pkey PRIMARY KEY (store_key, id);
              ALTER TABLE quality_snapshots DROP CONSTRAINT IF EXISTS quality_snapshots_pkey;
              ALTER TABLE quality_snapshots ADD CONSTRAINT quality_snapshots_pkey PRIMARY KEY (store_key, id);

              ALTER TABLE eval_artifacts
              ADD CONSTRAINT eval_artifacts_run_fk
              FOREIGN KEY (store_key, run_id) REFERENCES eval_runs(store_key, id)
              ON DELETE CASCADE;
              ALTER TABLE quality_snapshots
              ADD CONSTRAINT quality_snapshots_run_fk
              FOREIGN KEY (store_key, run_id) REFERENCES eval_runs(store_key, id)
              ON DELETE CASCADE;

              DROP INDEX IF EXISTS idx_eval_runs_updated;
              DROP INDEX IF EXISTS idx_eval_runs_pr;
              DROP INDEX IF EXISTS idx_eval_artifacts_run;
              DROP INDEX IF EXISTS idx_quality_snapshots_run;
              DROP INDEX IF EXISTS idx_quality_snapshots_pr;
              CREATE INDEX IF NOT EXISTS idx_eval_runs_store_updated
              ON eval_runs(store_key, updated_at DESC);
              CREATE INDEX IF NOT EXISTS idx_eval_runs_store_pr
              ON eval_runs(store_key, target_repo, target_pr_number, updated_at DESC);
              CREATE INDEX IF NOT EXISTS idx_eval_artifacts_store_run
              ON eval_artifacts(store_key, run_id, created_at);
              CREATE INDEX IF NOT EXISTS idx_quality_snapshots_store_run
              ON quality_snapshots(store_key, run_id, created_at DESC);
              CREATE INDEX IF NOT EXISTS idx_quality_snapshots_store_pr
              ON quality_snapshots(store_key, target_repo, target_pr_number, created_at DESC)",
    },
    Migration {
        version: 6,
        description: "record legacy eval store backfills",
        sql: "CREATE TABLE IF NOT EXISTS eval_store_legacy_backfills (
            store_key     TEXT NOT NULL DEFAULT current_schema(),
            legacy_schema TEXT NOT NULL,
            copied_rows   BIGINT NOT NULL DEFAULT 0,
            backfilled_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (store_key, legacy_schema)
        )",
    },
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalRunStatus {
    Created,
    Scored,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalRun {
    pub id: String,
    pub scenario: EvalScenario,
    pub target: EvalTarget,
    pub source_task_id: Option<String>,
    pub status: EvalRunStatus,
    pub quality_snapshot_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateEvalRun {
    pub scenario: EvalScenario,
    pub target: EvalTarget,
    pub source_task_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalArtifact {
    pub id: String,
    pub run_id: String,
    pub artifact_type: String,
    pub label: Option<String>,
    pub content_type: Option<String>,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AddEvalArtifact {
    pub artifact_type: String,
    pub label: Option<String>,
    pub content_type: Option<String>,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QualitySnapshotRecord {
    pub id: String,
    pub run_id: String,
    pub snapshot: QualitySnapshot,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalScoreResult {
    pub run: EvalRun,
    pub quality_snapshot: QualitySnapshotRecord,
}

pub struct EvalStore {
    pool: PgPool,
    schema: String,
    store_key: String,
}

impl EvalStore {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_database_url(path, None).await
    }

    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let context = PgStoreContext::from_path(path, configured_database_url)?;
        let schema = context.schema().to_owned();
        let store_key = schema.clone();
        let pool = context.open_migrated_pool(EVAL_MIGRATIONS).await?;
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
            .store_context(&StoreLocation::SharedSchema(EVAL_STORE_SCHEMA.to_string()))
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
            .open_migrated_pool_with_setup_pool(setup_pool, EVAL_MIGRATIONS)
            .await?;
        Ok(Self {
            pool,
            schema,
            store_key,
        })
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn store_key_for_data_dir(data_dir: &Path) -> anyhow::Result<String> {
        let canonical = data_dir.canonicalize().map_err(|error| {
            anyhow::anyhow!(
                "failed to canonicalize eval_store data_dir {}: {error}",
                data_dir.display()
            )
        })?;
        Ok(canonical.to_string_lossy().into_owned())
    }

    pub fn store_key(&self) -> &str {
        &self.store_key
    }

    pub async fn create_run(&self, input: CreateEvalRun) -> anyhow::Result<EvalRun> {
        let now = Utc::now();
        let run = EvalRun {
            id: Uuid::new_v4().to_string(),
            scenario: input.scenario,
            target: input.target,
            source_task_id: input.source_task_id,
            status: EvalRunStatus::Created,
            quality_snapshot_id: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
        };
        let data = serde_json::to_string(&run)?;
        let fields = target_fields(&run.target)?;
        sqlx::query(
            "INSERT INTO eval_runs
             (store_key, id, scenario, target_kind, target_repo, target_pr_number, target_issue_number,
              source_task_id, status, quality_snapshot_id, data, created_at, updated_at,
              completed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(&self.store_key)
        .bind(&run.id)
        .bind(scenario_key(&run.scenario))
        .bind(fields.kind)
        .bind(fields.repo)
        .bind(fields.pr_number)
        .bind(fields.issue_number)
        .bind(&run.source_task_id)
        .bind(status_key(&run.status))
        .bind(&run.quality_snapshot_id)
        .bind(&data)
        .bind(run.created_at)
        .bind(run.updated_at)
        .bind(run.completed_at)
        .execute(&self.pool)
        .await?;
        Ok(run)
    }

    pub async fn get_run(&self, run_id: &str) -> anyhow::Result<Option<EvalRun>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data FROM eval_runs WHERE store_key = $1 AND id = $2")
                .bind(&self.store_key)
                .bind(run_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn list_runs(&self, limit: i64) -> anyhow::Result<Vec<EvalRun>> {
        let limit = limit.clamp(1, 100);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data FROM eval_runs
                 WHERE store_key = $1
                 ORDER BY updated_at DESC LIMIT $2",
        )
        .bind(&self.store_key)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| serde_json::from_str(&data).map_err(Into::into))
            .collect()
    }

    pub async fn add_artifact(
        &self,
        run_id: &str,
        input: AddEvalArtifact,
    ) -> anyhow::Result<Option<EvalArtifact>> {
        if self.get_run(run_id).await?.is_none() {
            return Ok(None);
        }
        let artifact = EvalArtifact {
            id: Uuid::new_v4().to_string(),
            run_id: run_id.to_string(),
            artifact_type: input.artifact_type,
            label: input.label,
            content_type: input.content_type,
            body: input.body,
            created_at: Utc::now(),
        };
        let data = serde_json::to_string(&artifact)?;
        sqlx::query(
            "INSERT INTO eval_artifacts
             (store_key, id, run_id, artifact_type, label, content_type, body, data, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&self.store_key)
        .bind(&artifact.id)
        .bind(&artifact.run_id)
        .bind(&artifact.artifact_type)
        .bind(&artifact.label)
        .bind(&artifact.content_type)
        .bind(&artifact.body)
        .bind(&data)
        .bind(artifact.created_at)
        .execute(&self.pool)
        .await?;
        Ok(Some(artifact))
    }

    pub async fn list_artifacts(&self, run_id: &str) -> anyhow::Result<Vec<EvalArtifact>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data FROM eval_artifacts
             WHERE store_key = $1 AND run_id = $2
             ORDER BY created_at",
        )
        .bind(&self.store_key)
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| serde_json::from_str(&data).map_err(Into::into))
            .collect()
    }

    pub async fn score_run(
        &self,
        run_id: &str,
        input: PrRepairEvalInput,
    ) -> anyhow::Result<Option<EvalScoreResult>> {
        let Some(mut run) = self.get_run(run_id).await? else {
            return Ok(None);
        };
        if run.scenario != input.scenario || run.target != input.target {
            anyhow::bail!("eval input scenario and target must match the eval run");
        }
        let snapshot = score_pr_repair_eval(input)?;
        let now = Utc::now();
        let record = QualitySnapshotRecord {
            id: Uuid::new_v4().to_string(),
            run_id: run.id.clone(),
            snapshot,
            created_at: now,
        };
        run.status = EvalRunStatus::Scored;
        run.quality_snapshot_id = Some(record.id.clone());
        run.updated_at = now;
        run.completed_at = Some(now);
        let run_data = serde_json::to_string(&run)?;
        let record_data = serde_json::to_string(&record)?;
        let fields = target_fields(&run.target)?;

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO quality_snapshots
             (store_key, id, run_id, scenario, target_repo, target_pr_number, final_score, final_grade,
              data, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(&self.store_key)
        .bind(&record.id)
        .bind(&record.run_id)
        .bind(scenario_key(&run.scenario))
        .bind(fields.repo)
        .bind(fields.pr_number)
        .bind(i64::from(record.snapshot.final_score))
        .bind(format!("{:?}", record.snapshot.final_grade))
        .bind(&record_data)
        .bind(record.created_at)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE eval_runs
             SET status = $3, quality_snapshot_id = $4, data = $5, updated_at = $6,
                 completed_at = $7
             WHERE store_key = $1 AND id = $2",
        )
        .bind(&self.store_key)
        .bind(&run.id)
        .bind(status_key(&run.status))
        .bind(&run.quality_snapshot_id)
        .bind(&run_data)
        .bind(run.updated_at)
        .bind(run.completed_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(EvalScoreResult {
            run,
            quality_snapshot: record,
        }))
    }

    pub async fn get_quality_snapshot(
        &self,
        snapshot_id: &str,
    ) -> anyhow::Result<Option<QualitySnapshotRecord>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data FROM quality_snapshots WHERE store_key = $1 AND id = $2")
                .bind(&self.store_key)
                .bind(snapshot_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn list_quality_snapshots_for_pr(
        &self,
        repo: &str,
        pr_number: u64,
        limit: i64,
    ) -> anyhow::Result<Vec<QualitySnapshotRecord>> {
        let pr_number = i64::try_from(pr_number)?;
        let limit = limit.clamp(1, 100);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data FROM quality_snapshots
             WHERE store_key = $1 AND target_repo = $2 AND target_pr_number = $3
             ORDER BY created_at DESC
             LIMIT $4",
        )
        .bind(&self.store_key)
        .bind(repo)
        .bind(pr_number)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| serde_json::from_str(&data).map_err(Into::into))
            .collect()
    }
}

pub async fn migrate_legacy_eval_store_if_needed(
    legacy_path: &Path,
    configured_database_url: Option<&str>,
    target_store: &EvalStore,
) -> anyhow::Result<u64> {
    let legacy_context = PgStoreContext::from_path(legacy_path, configured_database_url)?;
    let legacy_schema = legacy_context.schema();
    if legacy_schema == target_store.schema() {
        return Ok(0);
    }

    let legacy_schema_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
             FROM pg_catalog.pg_namespace
             WHERE nspname = $1
         )",
    )
    .bind(legacy_schema)
    .fetch_one(&target_store.pool)
    .await?;
    if !legacy_schema_exists {
        return Ok(0);
    }

    let already_backfilled: Option<String> = sqlx::query_scalar(
        "SELECT legacy_schema
         FROM eval_store_legacy_backfills
         WHERE store_key = $1 AND legacy_schema = $2",
    )
    .bind(target_store.store_key())
    .bind(legacy_schema)
    .fetch_optional(&target_store.pool)
    .await?;
    if already_backfilled.is_some() {
        return Ok(0);
    }

    let legacy_runs: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("{}.eval_runs", quote_pg_ident(legacy_schema)))
        .fetch_one(&target_store.pool)
        .await?;
    if legacy_runs.is_none() {
        return Ok(0);
    }

    let legacy_pool = legacy_context.open_migrated_pool(EVAL_MIGRATIONS).await?;
    legacy_pool.close().await;

    let legacy_schema_sql = quote_pg_ident(legacy_schema);
    let mut copied = 0;
    let mut tx = target_store.pool.begin().await?;

    let copy_runs_sql = format!(
        "INSERT INTO eval_runs (
            store_key, id, scenario, target_kind, target_repo, target_pr_number,
            target_issue_number, source_task_id, status, quality_snapshot_id, data,
            created_at, updated_at, completed_at
         )
         SELECT $1, id, scenario, target_kind, target_repo, target_pr_number,
                target_issue_number, source_task_id, status, quality_snapshot_id, data,
                created_at, updated_at, completed_at
         FROM {legacy_schema_sql}.eval_runs
         ON CONFLICT (store_key, id) DO NOTHING"
    );
    copied += sqlx::query(&copy_runs_sql)
        .bind(target_store.store_key())
        .execute(&mut *tx)
        .await?
        .rows_affected();

    let copy_artifacts_sql = format!(
        "INSERT INTO eval_artifacts (
            store_key, id, run_id, artifact_type, label, content_type, body, data, created_at
         )
         SELECT $1, id, run_id, artifact_type, label, content_type, body, data, created_at
         FROM {legacy_schema_sql}.eval_artifacts
         ON CONFLICT (store_key, id) DO NOTHING"
    );
    copied += sqlx::query(&copy_artifacts_sql)
        .bind(target_store.store_key())
        .execute(&mut *tx)
        .await?
        .rows_affected();

    let copy_snapshots_sql = format!(
        "INSERT INTO quality_snapshots (
            store_key, id, run_id, scenario, target_repo, target_pr_number, final_score,
            final_grade, data, created_at
         )
         SELECT $1, id, run_id, scenario, target_repo, target_pr_number, final_score,
                final_grade, data, created_at
         FROM {legacy_schema_sql}.quality_snapshots
         ON CONFLICT (store_key, id) DO NOTHING"
    );
    copied += sqlx::query(&copy_snapshots_sql)
        .bind(target_store.store_key())
        .execute(&mut *tx)
        .await?
        .rows_affected();

    sqlx::query(
        "INSERT INTO eval_store_legacy_backfills (store_key, legacy_schema, copied_rows)
         VALUES ($1, $2, $3)
         ON CONFLICT (store_key, legacy_schema) DO NOTHING",
    )
    .bind(target_store.store_key())
    .bind(legacy_schema)
    .bind(copied as i64)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    if copied > 0 {
        tracing::info!(
            copied,
            legacy_schema,
            target_schema = target_store.schema(),
            "eval store migration: backfilled legacy rows into shared schema"
        );
    }

    Ok(copied)
}

fn quote_pg_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

struct TargetFields<'a> {
    kind: &'static str,
    repo: Option<&'a str>,
    pr_number: Option<i64>,
    issue_number: Option<i64>,
}

fn target_fields(target: &EvalTarget) -> anyhow::Result<TargetFields<'_>> {
    match target {
        EvalTarget::PullRequest {
            repo, pr_number, ..
        } => Ok(TargetFields {
            kind: "pull_request",
            repo: Some(repo.as_str()),
            pr_number: Some(i64::try_from(*pr_number)?),
            issue_number: None,
        }),
        EvalTarget::Issue { repo, issue_number } => Ok(TargetFields {
            kind: "issue",
            repo: Some(repo.as_str()),
            pr_number: None,
            issue_number: Some(i64::try_from(*issue_number)?),
        }),
        EvalTarget::PromptTask { .. } => Ok(TargetFields {
            kind: "prompt_task",
            repo: None,
            pr_number: None,
            issue_number: None,
        }),
    }
}

fn scenario_key(scenario: &EvalScenario) -> &'static str {
    match scenario {
        EvalScenario::PrRepair => "pr_repair",
        EvalScenario::ReadyNoopControl => "ready_noop_control",
    }
}

fn status_key(status: &EvalRunStatus) -> &'static str {
    match status {
        EvalRunStatus::Created => "created",
        EvalRunStatus::Scored => "scored",
    }
}

#[cfg(test)]
#[path = "eval_store_tests.rs"]
mod tests;
