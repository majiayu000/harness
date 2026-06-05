use chrono::{DateTime, Utc};
use harness_core::db::{Migration, PgStoreContext};
use harness_eval::{
    score_pr_repair_eval, EvalScenario, EvalTarget, PrRepairEvalInput, QualitySnapshot,
};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use std::path::Path;
use uuid::Uuid;

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
            created_at    TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
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
            created_at       TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
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
        let pool = context.open_migrated_pool(EVAL_MIGRATIONS).await?;
        Ok(Self { pool })
    }

    pub async fn open_with_context(
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, EVAL_MIGRATIONS)
            .await?;
        Ok(Self { pool })
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
             (id, scenario, target_kind, target_repo, target_pr_number, target_issue_number,
              source_task_id, status, quality_snapshot_id, data, created_at, updated_at,
              completed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
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
        let row: Option<(String,)> = sqlx::query_as("SELECT data FROM eval_runs WHERE id = $1")
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn list_runs(&self, limit: i64) -> anyhow::Result<Vec<EvalRun>> {
        let limit = limit.clamp(1, 100);
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT data FROM eval_runs ORDER BY updated_at DESC LIMIT $1")
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
             (id, run_id, artifact_type, label, content_type, body, data, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
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
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT data FROM eval_artifacts WHERE run_id = $1 ORDER BY created_at")
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
             (id, run_id, scenario, target_repo, target_pr_number, final_score, final_grade,
              data, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
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
             SET status = $2, quality_snapshot_id = $3, data = $4, updated_at = $5,
                 completed_at = $6
             WHERE id = $1",
        )
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
            sqlx::query_as("SELECT data FROM quality_snapshots WHERE id = $1")
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
             WHERE target_repo = $1 AND target_pr_number = $2
             ORDER BY created_at DESC
             LIMIT $3",
        )
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
