use chrono::{DateTime, Utc};
use harness_core::db::{
    pg_create_schema_if_not_exists, pg_open_pool, pg_open_pool_schematized, resolve_database_url,
    Migration, PgMigrator,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
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
    DispatchCompleted,
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
            ProjectWorkflowEventKind::DispatchStarted
            | ProjectWorkflowEventKind::DispatchCompleted => {
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

pub struct ProjectWorkflowStore {
    pool: PgPool,
    update_lock: tokio::sync::Mutex<()>,
}

impl ProjectWorkflowStore {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_database_url(path, None).await
    }

    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let database_url = resolve_database_url(configured_database_url)?;
        let path_utf8 = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {:?}", path))?;
        let digest = Sha256::digest(path_utf8.as_bytes());
        let mut schema_bytes = [0u8; 8];
        schema_bytes.copy_from_slice(&digest[..8]);
        let schema = format!("h{:016x}", u64::from_le_bytes(schema_bytes));

        let setup = pg_open_pool(&database_url).await?;
        pg_create_schema_if_not_exists(&setup, &schema).await?;
        setup.close().await;

        let pool = pg_open_pool_schematized(&database_url, &schema).await?;
        PgMigrator::new(&pool, PROJECT_WORKFLOW_MIGRATIONS)
            .run()
            .await?;
        Ok(Self {
            pool,
            update_lock: tokio::sync::Mutex::new(()),
        })
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

    pub async fn get_by_project(
        &self,
        project_id: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<Option<ProjectWorkflowInstance>> {
        let row: Option<(String,)> = if let Some(repo) = repo {
            sqlx::query_as(
                "SELECT data FROM project_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND data::jsonb->>'repo' = $2
                 ORDER BY updated_at DESC
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
        let _guard = self.update_lock.lock().await;
        let mut workflow = self
            .get_by_project(project_id, repo)
            .await?
            .unwrap_or_else(|| {
                ProjectWorkflowInstance::new(project_id.to_string(), repo.map(str::to_string))
            });
        if workflow.repo.is_none() {
            workflow.repo = repo.map(str::to_string);
        }
        f(&mut workflow);
        self.upsert(&workflow).await?;
        Ok(workflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open_test_store() -> anyhow::Result<Option<ProjectWorkflowStore>> {
        if std::env::var("DATABASE_URL").is_err() {
            return Ok(None);
        }
        let dir = tempfile::tempdir()?;
        match ProjectWorkflowStore::open(&dir.path().join("project_workflows.db")).await {
            Ok(store) => Ok(Some(store)),
            Err(e) => {
                tracing::warn!("project workflow store test skipped: {e}");
                Ok(None)
            }
        }
    }

    #[tokio::test]
    async fn project_workflow_store_tracks_repo_state_transitions() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let project_id = "/tmp/project-c";
        store
            .record_poll_started(project_id, Some("owner/repo"))
            .await?;
        store
            .record_planning_started(project_id, Some("owner/repo"))
            .await?;
        store
            .record_planner_enqueued(project_id, Some("owner/repo"), "planner-1")
            .await?;
        store
            .record_monitoring_started(project_id, Some("owner/repo"))
            .await?;

        let workflow = store
            .get_by_project(project_id, Some("owner/repo"))
            .await?
            .expect("project workflow");
        assert_eq!(workflow.state, ProjectWorkflowState::Monitoring);
        assert_eq!(
            workflow.active_planner_task_id.as_deref(),
            Some("planner-1")
        );
        Ok(())
    }

    #[tokio::test]
    async fn project_workflow_store_scopes_rows_by_repo() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let project_id = "/tmp/shared-project";
        store
            .record_poll_started(project_id, Some("owner/repo-a"))
            .await?;
        store
            .record_poll_started(project_id, Some("owner/repo-b"))
            .await?;

        let a = store
            .get_by_project(project_id, Some("owner/repo-a"))
            .await?
            .expect("repo-a workflow");
        let b = store
            .get_by_project(project_id, Some("owner/repo-b"))
            .await?
            .expect("repo-b workflow");

        assert_ne!(a.id, b.id);
        Ok(())
    }
}
