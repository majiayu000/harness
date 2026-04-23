use chrono::{DateTime, Utc};
use harness_core::db::{
    pg_create_schema_if_not_exists, pg_open_pool, pg_open_pool_schematized, resolve_database_url,
    Migration, PgMigrator,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use std::path::Path;

const ISSUE_WORKFLOW_SCHEMA_VERSION: u32 = 1;

static ISSUE_WORKFLOW_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create issue_workflows table",
        sql: "CREATE TABLE IF NOT EXISTS issue_workflows (
            id         TEXT PRIMARY KEY,
            data       TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 2,
        description: "index issue workflow lookups by project and issue",
        sql: "CREATE INDEX IF NOT EXISTS idx_issue_workflows_issue
              ON issue_workflows ((data::jsonb->>'project_id'), ((data::jsonb->>'issue_number')::bigint))",
    },
    Migration {
        version: 3,
        description: "index issue workflow lookups by project and pr",
        sql: "CREATE INDEX IF NOT EXISTS idx_issue_workflows_pr
              ON issue_workflows ((data::jsonb->>'project_id'), ((data::jsonb->>'pr_number')::bigint))",
    },
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueLifecycleState {
    Discovered,
    Scheduled,
    Implementing,
    PrOpen,
    AwaitingFeedback,
    AddressingFeedback,
    ReadyToMerge,
    Blocked,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueLifecycleEventKind {
    IssueScheduled,
    ImplementStarted,
    PlanIssueDetected,
    PrDetected,
    FeedbackTaskScheduled,
    FeedbackSweepCompleted,
    FeedbackFound,
    NoFeedbackFound,
    Mergeable,
    WorkflowFailed,
    WorkflowCancelled,
    WorkflowDone,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueLifecycleEvent {
    pub kind: IssueLifecycleEventKind,
    pub at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}

impl IssueLifecycleEvent {
    fn new(kind: IssueLifecycleEventKind) -> Self {
        Self {
            kind,
            at: Utc::now(),
            task_id: None,
            detail: None,
            pr_number: None,
            pr_url: None,
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

    fn with_pr(mut self, pr_number: u64, pr_url: impl Into<String>) -> Self {
        self.pr_number = Some(pr_number);
        self.pr_url = Some(pr_url.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueWorkflowInstance {
    pub id: String,
    pub schema_version: u32,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub issue_number: u64,
    pub state: IssueLifecycleState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub labels_snapshot: Vec<String>,
    #[serde(default)]
    pub force_execute: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_concern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event: Option<IssueLifecycleEvent>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl IssueWorkflowInstance {
    pub fn new(project_id: impl Into<String>, repo: Option<String>, issue_number: u64) -> Self {
        let project_id = project_id.into();
        let id = workflow_id(&project_id, issue_number);
        let now = Utc::now();
        Self {
            id,
            schema_version: ISSUE_WORKFLOW_SCHEMA_VERSION,
            project_id,
            repo,
            issue_number,
            state: IssueLifecycleState::Discovered,
            active_task_id: None,
            pr_number: None,
            pr_url: None,
            labels_snapshot: Vec::new(),
            force_execute: false,
            plan_concern: None,
            last_event: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn apply_event(&mut self, event: IssueLifecycleEvent) {
        match event.kind {
            IssueLifecycleEventKind::IssueScheduled => {
                self.state = IssueLifecycleState::Scheduled;
                self.active_task_id = event.task_id.clone();
            }
            IssueLifecycleEventKind::ImplementStarted => {
                self.state = IssueLifecycleState::Implementing;
                self.active_task_id = event.task_id.clone();
            }
            IssueLifecycleEventKind::PlanIssueDetected => {
                self.state = IssueLifecycleState::Implementing;
                self.active_task_id = event.task_id.clone();
                self.plan_concern = event.detail.clone();
            }
            IssueLifecycleEventKind::PrDetected => {
                self.state = IssueLifecycleState::PrOpen;
                self.active_task_id = event.task_id.clone();
                self.pr_number = event.pr_number;
                self.pr_url = event.pr_url.clone();
            }
            IssueLifecycleEventKind::FeedbackTaskScheduled
            | IssueLifecycleEventKind::FeedbackFound => {
                self.state = IssueLifecycleState::AddressingFeedback;
                self.active_task_id = event.task_id.clone();
                if event.pr_number.is_some() {
                    self.pr_number = event.pr_number;
                }
                if event.pr_url.is_some() {
                    self.pr_url = event.pr_url.clone();
                }
            }
            IssueLifecycleEventKind::FeedbackSweepCompleted
            | IssueLifecycleEventKind::NoFeedbackFound => {
                self.state = IssueLifecycleState::AwaitingFeedback;
            }
            IssueLifecycleEventKind::Mergeable => {
                self.state = IssueLifecycleState::ReadyToMerge;
            }
            IssueLifecycleEventKind::WorkflowFailed => {
                self.state = IssueLifecycleState::Failed;
            }
            IssueLifecycleEventKind::WorkflowCancelled => {
                self.state = IssueLifecycleState::Cancelled;
            }
            IssueLifecycleEventKind::WorkflowDone => {
                self.state = IssueLifecycleState::Done;
            }
        }
        self.last_event = Some(event);
        self.updated_at = Utc::now();
    }
}

pub fn workflow_id(project_id: &str, issue_number: u64) -> String {
    format!("{project_id}::issue:{issue_number}")
}

pub struct IssueWorkflowStore {
    pool: PgPool,
    update_lock: tokio::sync::Mutex<()>,
}

impl IssueWorkflowStore {
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
        PgMigrator::new(&pool, ISSUE_WORKFLOW_MIGRATIONS)
            .run()
            .await?;
        Ok(Self {
            pool,
            update_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub async fn upsert(&self, workflow: &IssueWorkflowInstance) -> anyhow::Result<()> {
        let data = serde_json::to_string(workflow)?;
        sqlx::query(
            "INSERT INTO issue_workflows (id, data) VALUES ($1, $2)
             ON CONFLICT(id) DO UPDATE SET data = EXCLUDED.data,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&workflow.id)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_by_issue(
        &self,
        project_id: &str,
        issue_number: u64,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data FROM issue_workflows
             WHERE data::jsonb->>'project_id' = $1
               AND (data::jsonb->>'issue_number')::bigint = $2
             ORDER BY updated_at DESC
             LIMIT 1",
        )
        .bind(project_id)
        .bind(issue_number as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn get_by_pr(
        &self,
        project_id: &str,
        pr_number: u64,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data FROM issue_workflows
             WHERE data::jsonb->>'project_id' = $1
               AND (data::jsonb->>'pr_number')::bigint = $2
             ORDER BY updated_at DESC
             LIMIT 1",
        )
        .bind(project_id)
        .bind(pr_number as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn record_issue_scheduled(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        task_id: &str,
        labels_snapshot: &[String],
        force_execute: bool,
    ) -> anyhow::Result<IssueWorkflowInstance> {
        self.update_issue(project_id, repo, issue_number, |workflow| {
            workflow.labels_snapshot = labels_snapshot.to_vec();
            workflow.force_execute = force_execute;
            workflow.apply_event(
                IssueLifecycleEvent::new(IssueLifecycleEventKind::IssueScheduled)
                    .with_task_id(task_id.to_string()),
            );
        })
        .await
    }

    pub async fn record_implement_started(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        task_id: &str,
    ) -> anyhow::Result<IssueWorkflowInstance> {
        self.update_issue(project_id, repo, issue_number, |workflow| {
            workflow.apply_event(
                IssueLifecycleEvent::new(IssueLifecycleEventKind::ImplementStarted)
                    .with_task_id(task_id.to_string()),
            );
        })
        .await
    }

    pub async fn record_plan_issue_detected(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        task_id: &str,
        concern: &str,
    ) -> anyhow::Result<IssueWorkflowInstance> {
        self.update_issue(project_id, repo, issue_number, |workflow| {
            workflow.apply_event(
                IssueLifecycleEvent::new(IssueLifecycleEventKind::PlanIssueDetected)
                    .with_task_id(task_id.to_string())
                    .with_detail(concern.to_string()),
            );
        })
        .await
    }

    pub async fn record_pr_detected(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        task_id: &str,
        pr_number: u64,
        pr_url: &str,
    ) -> anyhow::Result<IssueWorkflowInstance> {
        self.update_issue(project_id, repo, issue_number, |workflow| {
            workflow.apply_event(
                IssueLifecycleEvent::new(IssueLifecycleEventKind::PrDetected)
                    .with_task_id(task_id.to_string())
                    .with_pr(pr_number, pr_url.to_string()),
            );
        })
        .await
    }

    pub async fn record_feedback_task_scheduled(
        &self,
        project_id: &str,
        pr_number: u64,
        task_id: &str,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_by_pr(project_id, pr_number, |workflow| {
            let mut event =
                IssueLifecycleEvent::new(IssueLifecycleEventKind::FeedbackTaskScheduled)
                    .with_task_id(task_id.to_string());
            if let Some(pr_url) = workflow.pr_url.clone() {
                event = event.with_pr(pr_number, pr_url);
            }
            workflow.apply_event(event);
        })
        .await
    }

    pub async fn record_terminal_for_issue(
        &self,
        project_id: &str,
        issue_number: u64,
        final_state: IssueLifecycleState,
        detail: Option<&str>,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_existing_issue(project_id, issue_number, |workflow| {
            let mut event = IssueLifecycleEvent::new(match final_state {
                IssueLifecycleState::Done => IssueLifecycleEventKind::WorkflowDone,
                IssueLifecycleState::Cancelled => IssueLifecycleEventKind::WorkflowCancelled,
                _ => IssueLifecycleEventKind::WorkflowFailed,
            });
            if let Some(detail) = detail {
                event = event.with_detail(detail.to_string());
            }
            workflow.apply_event(event);
        })
        .await
    }

    pub async fn record_terminal_for_pr(
        &self,
        project_id: &str,
        pr_number: u64,
        success: bool,
        detail: Option<&str>,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_by_pr(project_id, pr_number, |workflow| {
            let mut event = IssueLifecycleEvent::new(if success {
                IssueLifecycleEventKind::Mergeable
            } else {
                IssueLifecycleEventKind::WorkflowFailed
            });
            if let Some(detail) = detail {
                event = event.with_detail(detail.to_string());
            }
            workflow.apply_event(event);
        })
        .await
    }

    pub async fn list_feedback_candidates(&self) -> anyhow::Result<Vec<IssueWorkflowInstance>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data FROM issue_workflows
             WHERE data::jsonb->>'state' IN ('pr_open', 'awaiting_feedback')
               AND data::jsonb->>'pr_number' IS NOT NULL
             ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    async fn update_issue<F>(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        f: F,
    ) -> anyhow::Result<IssueWorkflowInstance>
    where
        F: FnOnce(&mut IssueWorkflowInstance),
    {
        let _guard = self.update_lock.lock().await;
        let mut workflow = self
            .get_by_issue(project_id, issue_number)
            .await?
            .unwrap_or_else(|| {
                IssueWorkflowInstance::new(
                    project_id.to_string(),
                    repo.map(|r| r.to_string()),
                    issue_number,
                )
            });
        if workflow.repo.is_none() {
            workflow.repo = repo.map(|r| r.to_string());
        }
        f(&mut workflow);
        self.upsert(&workflow).await?;
        Ok(workflow)
    }

    async fn update_existing_issue<F>(
        &self,
        project_id: &str,
        issue_number: u64,
        f: F,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>>
    where
        F: FnOnce(&mut IssueWorkflowInstance),
    {
        let _guard = self.update_lock.lock().await;
        let Some(mut workflow) = self.get_by_issue(project_id, issue_number).await? else {
            return Ok(None);
        };
        f(&mut workflow);
        self.upsert(&workflow).await?;
        Ok(Some(workflow))
    }

    async fn update_by_pr<F>(
        &self,
        project_id: &str,
        pr_number: u64,
        f: F,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>>
    where
        F: FnOnce(&mut IssueWorkflowInstance),
    {
        let _guard = self.update_lock.lock().await;
        let Some(mut workflow) = self.get_by_pr(project_id, pr_number).await? else {
            return Ok(None);
        };
        f(&mut workflow);
        self.upsert(&workflow).await?;
        Ok(Some(workflow))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open_test_store() -> anyhow::Result<Option<IssueWorkflowStore>> {
        if std::env::var("DATABASE_URL").is_err() {
            return Ok(None);
        }
        let dir = tempfile::tempdir()?;
        match IssueWorkflowStore::open(&dir.path().join("issue_workflows.db")).await {
            Ok(store) => Ok(Some(store)),
            Err(e) => {
                tracing::warn!("issue workflow store test skipped: {e}");
                Ok(None)
            }
        }
    }

    #[tokio::test]
    async fn issue_workflow_store_binds_pr_to_issue_instance() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let project_id = "/tmp/project-a";
        store
            .record_issue_scheduled(
                project_id,
                Some("owner/repo"),
                882,
                "task-1",
                &["force-execute".to_string()],
                true,
            )
            .await?;
        store
            .record_pr_detected(
                project_id,
                Some("owner/repo"),
                882,
                "task-1",
                909,
                "https://github.com/owner/repo/pull/909",
            )
            .await?;

        let by_issue = store
            .get_by_issue(project_id, 882)
            .await?
            .expect("workflow by issue");
        let by_pr = store
            .get_by_pr(project_id, 909)
            .await?
            .expect("workflow by pr");

        assert_eq!(by_issue.id, by_pr.id);
        assert_eq!(by_issue.state, IssueLifecycleState::PrOpen);
        assert_eq!(by_issue.pr_number, Some(909));
        assert!(by_issue.force_execute);
        assert_eq!(by_issue.labels_snapshot, vec!["force-execute".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn issue_workflow_store_records_plan_issue_as_non_terminal_event() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let project_id = "/tmp/project-b";
        store
            .record_issue_scheduled(project_id, Some("owner/repo"), 883, "task-2", &[], false)
            .await?;
        let workflow = store
            .record_plan_issue_detected(
                project_id,
                Some("owner/repo"),
                883,
                "task-2",
                "The plan is incomplete.",
            )
            .await?;

        assert_eq!(workflow.state, IssueLifecycleState::Implementing);
        assert_eq!(
            workflow.plan_concern.as_deref(),
            Some("The plan is incomplete.")
        );
        assert_eq!(
            workflow.last_event.as_ref().map(|e| &e.kind),
            Some(&IssueLifecycleEventKind::PlanIssueDetected)
        );
        Ok(())
    }
}
