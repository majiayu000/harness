use chrono::{DateTime, Utc};
use harness_core::db::{Migration, PgStoreContext};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use sqlx::Postgres;
use std::path::Path;

const PROJECT_WORKFLOW_SCHEMA_VERSION: u32 = 1;

static PROJECT_WORKFLOW_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create project_workflows table",
        sql: "CREATE TABLE IF NOT EXISTS project_workflows (
            id         TEXT PRIMARY KEY,
            data       TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 2,
        description: "index project workflow lookups by project",
        sql: "CREATE INDEX IF NOT EXISTS idx_project_workflows_project
              ON project_workflows ((data::jsonb->>'project_id'))",
    },
    Migration {
        version: 3,
        description: "index case-insensitive GitHub project workflow lookups",
        sql: "CREATE INDEX IF NOT EXISTS idx_project_workflows_repo_ci
              ON project_workflows (
                  (data::jsonb->>'project_id'),
                  (LOWER(data::jsonb->>'repo')),
                  updated_at DESC
              )",
    },
    Migration {
        version: 4,
        description: "enforce unique project workflow repository identities",
        sql: "DO $$
              BEGIN
                IF EXISTS (
                  SELECT 1 FROM project_workflows
                  WHERE data::jsonb->>'repo' IS NOT NULL
                  GROUP BY data::jsonb->>'project_id', LOWER(data::jsonb->>'repo')
                  HAVING COUNT(*) > 1
                ) THEN
                  RAISE EXCEPTION 'project_workflows contains case-colliding repository identities; resolve duplicates before migration';
                END IF;
              END $$;
              DROP INDEX IF EXISTS idx_project_workflows_repo_ci;
              CREATE UNIQUE INDEX idx_project_workflows_repo_ci
              ON project_workflows (
                  (data::jsonb->>'project_id'),
                  (LOWER(data::jsonb->>'repo'))
              )
              WHERE data::jsonb->>'repo' IS NOT NULL",
    },
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectWorkflowState {
    Idle,
    PollingIntake,
    PlanningBatch,
    Dispatching,
    Monitoring,
    SweepingFeedback,
    Paused,
    Degraded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectWorkflowEventKind {
    PollStarted,
    PollCompleted,
    SprintPlanningStarted,
    SprintPlannerEnqueued,
    DispatchStarted,
    MonitoringStarted,
    FeedbackSweepStarted,
    FeedbackSweepCompleted,
    RepoPaused,
    RepoDegraded,
    RepoIdle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectWorkflowEvent {
    pub kind: ProjectWorkflowEventKind,
    pub at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ProjectWorkflowEvent {
    fn new(kind: ProjectWorkflowEventKind) -> Self {
        Self {
            kind,
            at: Utc::now(),
            task_id: None,
            detail: None,
        }
    }

    fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectWorkflowInstance {
    pub id: String,
    pub schema_version: u32,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub state: ProjectWorkflowState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_planner_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event: Option<ProjectWorkflowEvent>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ProjectWorkflowInstance {
    pub fn new(project_id: impl Into<String>, repo: Option<String>) -> Self {
        let project_id = project_id.into();
        let repo = repo.map(|repo| repo.to_ascii_lowercase());
        let now = Utc::now();
        Self {
            id: workflow_id(&project_id, repo.as_deref()),
            schema_version: PROJECT_WORKFLOW_SCHEMA_VERSION,
            project_id,
            repo,
            state: ProjectWorkflowState::Idle,
            active_planner_task_id: None,
            degraded_reason: None,
            last_event: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn apply_event(&mut self, event: ProjectWorkflowEvent) {
        match event.kind {
            ProjectWorkflowEventKind::PollStarted => {
                self.state = ProjectWorkflowState::PollingIntake;
            }
            ProjectWorkflowEventKind::PollCompleted => {
                self.state = ProjectWorkflowState::Idle;
            }
            ProjectWorkflowEventKind::SprintPlanningStarted => {
                self.state = ProjectWorkflowState::PlanningBatch;
            }
            ProjectWorkflowEventKind::SprintPlannerEnqueued => {
                self.state = ProjectWorkflowState::PlanningBatch;
                self.active_planner_task_id = event.task_id.clone();
            }
            ProjectWorkflowEventKind::DispatchStarted => {
                self.state = ProjectWorkflowState::Dispatching;
            }
            ProjectWorkflowEventKind::MonitoringStarted => {
                self.state = ProjectWorkflowState::Monitoring;
            }
            ProjectWorkflowEventKind::FeedbackSweepStarted => {
                self.state = ProjectWorkflowState::SweepingFeedback;
            }
            ProjectWorkflowEventKind::FeedbackSweepCompleted
            | ProjectWorkflowEventKind::RepoIdle => {
                self.state = ProjectWorkflowState::Idle;
                self.degraded_reason = None;
            }
            ProjectWorkflowEventKind::RepoPaused => {
                self.state = ProjectWorkflowState::Paused;
            }
            ProjectWorkflowEventKind::RepoDegraded => {
                self.state = ProjectWorkflowState::Degraded;
                self.degraded_reason = event.detail.clone();
            }
        }
        self.last_event = Some(event);
        self.updated_at = Utc::now();
    }
}

fn repo_key(repo: Option<&str>) -> &str {
    repo.unwrap_or("<none>")
}

pub fn workflow_id(project_id: &str, repo: Option<&str>) -> String {
    format!("{project_id}::repo:{}::project", repo_key(repo))
}

pub fn legacy_schema_for_path(path: &Path) -> anyhow::Result<String> {
    let path_utf8 = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {:?}", path))?;
    let digest = Sha256::digest(path_utf8.as_bytes());
    let mut schema_bytes = [0u8; 8];
    schema_bytes.copy_from_slice(&digest[..8]);
    Ok(format!("h{:016x}", u64::from_le_bytes(schema_bytes)))
}

pub struct ProjectWorkflowStore {
    pool: PgPool,
}

impl ProjectWorkflowStore {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_database_url(path, None).await
    }

    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let context =
            PgStoreContext::from_schema(&legacy_schema_for_path(path)?, configured_database_url)?;
        let pool = context
            .open_migrated_pool(PROJECT_WORKFLOW_MIGRATIONS)
            .await?;
        Ok(Self { pool })
    }

    pub async fn open_with_database_url_and_schema(
        configured_database_url: Option<&str>,
        schema: &str,
    ) -> anyhow::Result<Self> {
        let context = PgStoreContext::from_schema(schema, configured_database_url)?;
        let pool = context
            .open_migrated_pool(PROJECT_WORKFLOW_MIGRATIONS)
            .await?;
        Ok(Self { pool })
    }

    pub async fn open_with_context(
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, PROJECT_WORKFLOW_MIGRATIONS)
            .await?;
        Ok(Self { pool })
    }

    pub async fn upsert(&self, workflow: &ProjectWorkflowInstance) -> anyhow::Result<()> {
        let data = serde_json::to_string(workflow)?;
        sqlx::query(
            "INSERT INTO project_workflows (id, data) VALUES ($1, $2)
             ON CONFLICT(id) DO UPDATE SET data = EXCLUDED.data,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&workflow.id)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_if_absent(
        &self,
        workflow: &ProjectWorkflowInstance,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        lock_project_identity(&mut tx, &workflow.project_id, workflow.repo.as_deref()).await?;
        if self
            .load_for_update_by_project(&mut tx, &workflow.project_id, workflow.repo.as_deref())
            .await?
            .is_some()
        {
            tx.commit().await?;
            return Ok(false);
        }
        let data = serde_json::to_string(workflow)?;
        let result = sqlx::query(
            "INSERT INTO project_workflows (id, data) VALUES ($1, $2)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(&workflow.id)
        .bind(&data)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn get_by_project(
        &self,
        project_id: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<Option<ProjectWorkflowInstance>> {
        let row: Option<(String,)> = if let Some(repo) = repo {
            sqlx::query_as(
                "SELECT data FROM project_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND LOWER(data::jsonb->>'repo') = LOWER($2)
                 ORDER BY (data::jsonb->>'repo' = $2) DESC, updated_at DESC
                 LIMIT 1",
            )
            .bind(project_id)
            .bind(repo)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_as(
                "SELECT data FROM project_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND data::jsonb->>'repo' IS NULL
                 ORDER BY updated_at DESC
                 LIMIT 1",
            )
            .bind(project_id)
            .fetch_optional(&self.pool)
            .await?
        };
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn list(&self) -> anyhow::Result<Vec<ProjectWorkflowInstance>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT data FROM project_workflows ORDER BY updated_at DESC")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn row_count(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM project_workflows")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    pub async fn record_poll_started(
        &self,
        project_id: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<ProjectWorkflowInstance> {
        self.update(project_id, repo, |workflow| {
            workflow.apply_event(ProjectWorkflowEvent::new(
                ProjectWorkflowEventKind::PollStarted,
            ));
        })
        .await
    }

    pub async fn record_planning_started(
        &self,
        project_id: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<ProjectWorkflowInstance> {
        self.update(project_id, repo, |workflow| {
            workflow.apply_event(ProjectWorkflowEvent::new(
                ProjectWorkflowEventKind::SprintPlanningStarted,
            ));
        })
        .await
    }

    pub async fn record_planner_enqueued(
        &self,
        project_id: &str,
        repo: Option<&str>,
        task_id: &str,
    ) -> anyhow::Result<ProjectWorkflowInstance> {
        self.update(project_id, repo, |workflow| {
            workflow.apply_event(
                ProjectWorkflowEvent::new(ProjectWorkflowEventKind::SprintPlannerEnqueued)
                    .with_task_id(task_id.to_string()),
            );
        })
        .await
    }

    pub async fn record_dispatch_started(
        &self,
        project_id: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<ProjectWorkflowInstance> {
        self.update(project_id, repo, |workflow| {
            workflow.apply_event(ProjectWorkflowEvent::new(
                ProjectWorkflowEventKind::DispatchStarted,
            ));
        })
        .await
    }

    pub async fn record_monitoring_started(
        &self,
        project_id: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<ProjectWorkflowInstance> {
        self.update(project_id, repo, |workflow| {
            workflow.apply_event(ProjectWorkflowEvent::new(
                ProjectWorkflowEventKind::MonitoringStarted,
            ));
        })
        .await
    }

    pub async fn record_feedback_sweep_started(
        &self,
        project_id: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<ProjectWorkflowInstance> {
        self.update(project_id, repo, |workflow| {
            workflow.apply_event(ProjectWorkflowEvent::new(
                ProjectWorkflowEventKind::FeedbackSweepStarted,
            ));
        })
        .await
    }

    pub async fn record_feedback_sweep_completed(
        &self,
        project_id: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<ProjectWorkflowInstance> {
        self.update(project_id, repo, |workflow| {
            workflow.apply_event(ProjectWorkflowEvent::new(
                ProjectWorkflowEventKind::FeedbackSweepCompleted,
            ));
        })
        .await
    }

    pub async fn record_idle(
        &self,
        project_id: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<ProjectWorkflowInstance> {
        self.update(project_id, repo, |workflow| {
            workflow.apply_event(ProjectWorkflowEvent::new(
                ProjectWorkflowEventKind::RepoIdle,
            ));
        })
        .await
    }

    pub async fn record_paused(
        &self,
        project_id: &str,
        repo: Option<&str>,
        detail: &str,
    ) -> anyhow::Result<ProjectWorkflowInstance> {
        self.update(project_id, repo, |workflow| {
            workflow.apply_event(
                ProjectWorkflowEvent::new(ProjectWorkflowEventKind::RepoPaused)
                    .with_detail(detail.to_string()),
            );
        })
        .await
    }

    pub async fn record_degraded(
        &self,
        project_id: &str,
        repo: Option<&str>,
        detail: &str,
    ) -> anyhow::Result<ProjectWorkflowInstance> {
        self.update(project_id, repo, |workflow| {
            workflow.apply_event(
                ProjectWorkflowEvent::new(ProjectWorkflowEventKind::RepoDegraded)
                    .with_detail(detail.to_string()),
            );
        })
        .await
    }

    async fn update<F>(
        &self,
        project_id: &str,
        repo: Option<&str>,
        f: F,
    ) -> anyhow::Result<ProjectWorkflowInstance>
    where
        F: FnOnce(&mut ProjectWorkflowInstance),
    {
        let mut tx = self.pool.begin().await?;
        lock_project_identity(&mut tx, project_id, repo).await?;
        if let Some((row_id, mut workflow)) = self
            .load_for_update_by_project(&mut tx, project_id, repo)
            .await?
        {
            f(&mut workflow);
            self.upsert_in_tx(&mut tx, &workflow).await?;
            debug_assert_eq!(workflow.id, row_id);
            tx.commit().await?;
            return Ok(workflow);
        }
        let placeholder =
            ProjectWorkflowInstance::new(project_id.to_string(), repo.map(str::to_string));
        let workflow_id = placeholder.id.clone();
        self.insert_placeholder(&mut tx, &workflow_id, &placeholder)
            .await?;
        let mut workflow = self
            .load_for_update_by_id(&mut tx, &workflow_id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("project workflow row disappeared after placeholder insert")
            })?;
        if workflow.repo.is_none() {
            workflow.repo = repo.map(str::to_string);
        }
        f(&mut workflow);
        self.upsert_in_tx(&mut tx, &workflow).await?;
        tx.commit().await?;
        Ok(workflow)
    }

    async fn insert_placeholder(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        workflow_id: &str,
        workflow: &ProjectWorkflowInstance,
    ) -> anyhow::Result<()> {
        let data = serde_json::to_string(workflow)?;
        sqlx::query(
            "INSERT INTO project_workflows (id, data) VALUES ($1, $2)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(workflow_id)
        .bind(&data)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn load_for_update_by_id(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        workflow_id: &str,
    ) -> anyhow::Result<Option<ProjectWorkflowInstance>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data FROM project_workflows WHERE id = $1 FOR UPDATE")
                .bind(workflow_id)
                .fetch_optional(&mut **tx)
                .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    async fn load_for_update_by_project(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        project_id: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<Option<(String, ProjectWorkflowInstance)>> {
        let row: Option<(String, String)> = if let Some(repo) = repo {
            sqlx::query_as(
                "SELECT id, data FROM project_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND LOWER(data::jsonb->>'repo') = LOWER($2)
                 ORDER BY (data::jsonb->>'repo' = $2) DESC, updated_at DESC
                 LIMIT 1
                 FOR UPDATE",
            )
            .bind(project_id)
            .bind(repo)
            .fetch_optional(&mut **tx)
            .await?
        } else {
            sqlx::query_as(
                "SELECT id, data FROM project_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND data::jsonb->>'repo' IS NULL
                 ORDER BY updated_at DESC
                 LIMIT 1
                 FOR UPDATE",
            )
            .bind(project_id)
            .fetch_optional(&mut **tx)
            .await?
        };
        row.map(|(id, data)| Ok((id, serde_json::from_str(&data)?)))
            .transpose()
    }

    async fn upsert_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        workflow: &ProjectWorkflowInstance,
    ) -> anyhow::Result<()> {
        let data = serde_json::to_string(workflow)?;
        sqlx::query(
            "UPDATE project_workflows
             SET data = $1, updated_at = CURRENT_TIMESTAMP
             WHERE id = $2",
        )
        .bind(&data)
        .bind(&workflow.id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

async fn lock_project_identity(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    project_id: &str,
    repo: Option<&str>,
) -> anyhow::Result<()> {
    let canonical_repo = repo.map(str::to_ascii_lowercase);
    let lock_key =
        serde_json::to_string(&("project_workflow_identity", project_id, canonical_repo))?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
#[path = "project_lifecycle_migration_tests.rs"]
mod migration_tests;

#[cfg(test)]
#[path = "project_lifecycle_tests.rs"]
mod tests;
