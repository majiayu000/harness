use chrono::{DateTime, Utc};
use harness_core::db::{
    pg_create_schema_if_not_exists, pg_open_pool, pg_open_pool_schematized, resolve_database_url,
    Migration, PgMigrator,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use sqlx::Postgres;
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
    Migration {
        version: 4,
        description: "index issue workflow feedback sweep candidates",
        sql: "CREATE INDEX IF NOT EXISTS idx_issue_workflows_feedback_candidates
              ON issue_workflows (((data::jsonb->>'state')), updated_at DESC)
              WHERE data::jsonb->>'pr_number' IS NOT NULL",
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
    pub feedback_claimed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event: Option<IssueLifecycleEvent>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl IssueWorkflowInstance {
    pub fn new(project_id: impl Into<String>, repo: Option<String>, issue_number: u64) -> Self {
        let project_id = project_id.into();
        let id = workflow_id(&project_id, repo.as_deref(), issue_number);
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
            feedback_claimed_at: None,
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
                self.feedback_claimed_at = Some(Utc::now());
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
                self.active_task_id = None;
                self.feedback_claimed_at = None;
            }
            IssueLifecycleEventKind::Mergeable => {
                self.state = IssueLifecycleState::ReadyToMerge;
                self.feedback_claimed_at = None;
            }
            IssueLifecycleEventKind::WorkflowFailed => {
                self.state = IssueLifecycleState::Failed;
                self.feedback_claimed_at = None;
            }
            IssueLifecycleEventKind::WorkflowCancelled => {
                self.state = IssueLifecycleState::Cancelled;
                self.feedback_claimed_at = None;
            }
            IssueLifecycleEventKind::WorkflowDone => {
                self.state = IssueLifecycleState::Done;
                self.feedback_claimed_at = None;
            }
        }
        self.last_event = Some(event);
        self.updated_at = Utc::now();
    }
}

fn repo_key(repo: Option<&str>) -> &str {
    repo.unwrap_or("<none>")
}

pub fn workflow_id(project_id: &str, repo: Option<&str>, issue_number: u64) -> String {
    format!(
        "{project_id}::repo:{}::issue:{issue_number}",
        repo_key(repo)
    )
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

pub struct IssueWorkflowStore {
    pool: PgPool,
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
        let schema = legacy_schema_for_path(path)?;

        let setup = pg_open_pool(&database_url).await?;
        pg_create_schema_if_not_exists(&setup, &schema).await?;
        setup.close().await;

        let pool = pg_open_pool_schematized(&database_url, &schema).await?;
        PgMigrator::new(&pool, ISSUE_WORKFLOW_MIGRATIONS)
            .run()
            .await?;
        Ok(Self { pool })
    }

    pub async fn open_with_database_url_and_schema(
        configured_database_url: Option<&str>,
        schema: &str,
    ) -> anyhow::Result<Self> {
        let database_url = resolve_database_url(configured_database_url)?;
        let setup = pg_open_pool(&database_url).await?;
        pg_create_schema_if_not_exists(&setup, schema).await?;
        setup.close().await;

        let pool = pg_open_pool_schematized(&database_url, schema).await?;
        PgMigrator::new(&pool, ISSUE_WORKFLOW_MIGRATIONS)
            .run()
            .await?;
        Ok(Self { pool })
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
        repo: Option<&str>,
        issue_number: u64,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        let row: Option<(String,)> = if let Some(repo) = repo {
            sqlx::query_as(
                "SELECT data FROM issue_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND data::jsonb->>'repo' = $2
                   AND (data::jsonb->>'issue_number')::bigint = $3
                 ORDER BY updated_at DESC
                 LIMIT 1",
            )
            .bind(project_id)
            .bind(repo)
            .bind(issue_number as i64)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_as(
                "SELECT data FROM issue_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND data::jsonb->>'repo' IS NULL
                   AND (data::jsonb->>'issue_number')::bigint = $2
                 ORDER BY updated_at DESC
                 LIMIT 1",
            )
            .bind(project_id)
            .bind(issue_number as i64)
            .fetch_optional(&self.pool)
            .await?
        };
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn get_by_pr(
        &self,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        let row: Option<(String,)> = if let Some(repo) = repo {
            sqlx::query_as(
                "SELECT data FROM issue_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND data::jsonb->>'repo' = $2
                   AND (data::jsonb->>'pr_number')::bigint = $3
                 ORDER BY updated_at DESC
                 LIMIT 1",
            )
            .bind(project_id)
            .bind(repo)
            .bind(pr_number as i64)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_as(
                "SELECT data FROM issue_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND data::jsonb->>'repo' IS NULL
                   AND (data::jsonb->>'pr_number')::bigint = $2
                 ORDER BY updated_at DESC
                 LIMIT 1",
            )
            .bind(project_id)
            .bind(pr_number as i64)
            .fetch_optional(&self.pool)
            .await?
        };
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn list(&self) -> anyhow::Result<Vec<IssueWorkflowInstance>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT data FROM issue_workflows ORDER BY updated_at DESC")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn row_count(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM issue_workflows")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
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
        repo: Option<&str>,
        pr_number: u64,
        task_id: &str,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_by_pr(project_id, repo, pr_number, |workflow| {
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
        repo: Option<&str>,
        issue_number: u64,
        final_state: IssueLifecycleState,
        detail: Option<&str>,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_existing_issue(project_id, repo, issue_number, |workflow| {
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
        repo: Option<&str>,
        pr_number: u64,
        success: bool,
        cancelled: bool,
        detail: Option<&str>,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_by_pr(project_id, repo, pr_number, |workflow| {
            let mut event = IssueLifecycleEvent::new(if cancelled {
                IssueLifecycleEventKind::WorkflowCancelled
            } else if success {
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

    pub async fn claim_feedback_candidates(
        &self,
        limit: i64,
        stale_before: DateTime<Utc>,
    ) -> anyhow::Result<Vec<IssueWorkflowInstance>> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, data FROM issue_workflows
             WHERE data::jsonb->>'pr_number' IS NOT NULL
               AND (
                    data::jsonb->>'state' IN ('pr_open', 'awaiting_feedback')
                    OR (
                        data::jsonb->>'state' = 'addressing_feedback'
                        AND updated_at <= $2
                    )
               )
             ORDER BY updated_at DESC
             LIMIT $1
             FOR UPDATE SKIP LOCKED",
        )
        .bind(limit)
        .bind(stale_before)
        .fetch_all(&mut *tx)
        .await?;

        let mut claimed = Vec::with_capacity(rows.len());
        for (workflow_id, data) in rows {
            let mut workflow: IssueWorkflowInstance = match serde_json::from_str(&data) {
                Ok(workflow) => workflow,
                Err(e) => {
                    tracing::warn!(
                        workflow_id = %workflow_id,
                        "workflow feedback sweep: skipping malformed workflow row while claiming candidates: {e}"
                    );
                    continue;
                }
            };
            let claim_id = format!("claim:{}", workflow.id);
            if let Some(pr_number) = workflow.pr_number {
                let pr_url = workflow.pr_url.clone().unwrap_or_default();
                workflow.apply_event(
                    IssueLifecycleEvent::new(IssueLifecycleEventKind::FeedbackTaskScheduled)
                        .with_task_id(claim_id)
                        .with_pr(pr_number, pr_url),
                );
                self.upsert_in_tx(&mut tx, &workflow).await?;
                debug_assert_eq!(workflow.id, workflow_id);
                claimed.push(workflow);
            }
        }
        tx.commit().await?;
        Ok(claimed)
    }

    pub async fn release_feedback_claim(
        &self,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
        detail: &str,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_by_pr(project_id, repo, pr_number, |workflow| {
            workflow.apply_event(
                IssueLifecycleEvent::new(IssueLifecycleEventKind::NoFeedbackFound)
                    .with_detail(detail.to_string()),
            );
        })
        .await
    }

    /// Patches only the `project_id` field inside the JSONB blob without replacing the whole row.
    ///
    /// WARNING: the row primary key (`id`) is NOT updated by this function. The `id` column embeds
    /// the project_id, so after this patch the primary key will be stale and diverge from the JSON
    /// `project_id` field. Subsequent lookups via the canonical path will compute a different
    /// `workflow_id`, causing `ON CONFLICT DO NOTHING` to create duplicate orphan rows.
    ///
    /// Callers that resolve a canonical path MUST use [`IssueWorkflowStore::repair_project_id`]
    /// instead, which performs a correct delete-then-insert in a single transaction.
    ///
    /// Returns `Ok(())` even when no row matches `workflow_id` (zero rows affected is not an error).
    pub async fn update_project_path(
        &self,
        workflow_id: &str,
        new_path: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE issue_workflows
             SET    data = jsonb_set(data::jsonb, '{project_id}', to_jsonb($2::text), false)::text,
                    updated_at = CURRENT_TIMESTAMP
             WHERE  id = $1",
        )
        .bind(workflow_id)
        .bind(new_path)
        .execute(&self.pool)
        .await?;
        Ok(())
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
        let workflow_id = workflow_id(project_id, repo, issue_number);
        let placeholder = IssueWorkflowInstance::new(
            project_id.to_string(),
            repo.map(|r| r.to_string()),
            issue_number,
        );
        let mut tx = self.pool.begin().await?;
        self.insert_placeholder(&mut tx, &workflow_id, &placeholder)
            .await?;
        let mut workflow = self
            .load_for_update_by_id(&mut tx, &workflow_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("workflow row disappeared after placeholder insert"))?;
        if workflow.repo.is_none() {
            workflow.repo = repo.map(|r| r.to_string());
        }
        f(&mut workflow);
        self.upsert_in_tx(&mut tx, &workflow).await?;
        tx.commit().await?;
        Ok(workflow)
    }

    async fn update_existing_issue<F>(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        f: F,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>>
    where
        F: FnOnce(&mut IssueWorkflowInstance),
    {
        let mut tx = self.pool.begin().await?;
        let workflow_id = workflow_id(project_id, repo, issue_number);
        let Some(mut workflow) = self.load_for_update_by_id(&mut tx, &workflow_id).await? else {
            return Ok(None);
        };
        f(&mut workflow);
        self.upsert_in_tx(&mut tx, &workflow).await?;
        tx.commit().await?;
        Ok(Some(workflow))
    }

    async fn update_by_pr<F>(
        &self,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
        f: F,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>>
    where
        F: FnOnce(&mut IssueWorkflowInstance),
    {
        let mut tx = self.pool.begin().await?;
        let Some((workflow_id, mut workflow)) = self
            .load_for_update_by_pr(&mut tx, project_id, repo, pr_number)
            .await?
        else {
            return Ok(None);
        };
        f(&mut workflow);
        self.upsert_in_tx(&mut tx, &workflow).await?;
        debug_assert_eq!(workflow.id, workflow_id);
        tx.commit().await?;
        Ok(Some(workflow))
    }

    async fn insert_placeholder(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        workflow_id: &str,
        workflow: &IssueWorkflowInstance,
    ) -> anyhow::Result<()> {
        let data = serde_json::to_string(workflow)?;
        sqlx::query(
            "INSERT INTO issue_workflows (id, data) VALUES ($1, $2)
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
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data FROM issue_workflows WHERE id = $1 FOR UPDATE")
                .bind(workflow_id)
                .fetch_optional(&mut **tx)
                .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    async fn load_for_update_by_pr(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
    ) -> anyhow::Result<Option<(String, IssueWorkflowInstance)>> {
        let row: Option<(String, String)> = if let Some(repo) = repo {
            sqlx::query_as(
                "SELECT id, data FROM issue_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND data::jsonb->>'repo' = $2
                   AND (data::jsonb->>'pr_number')::bigint = $3
                 ORDER BY updated_at DESC
                 LIMIT 1
                 FOR UPDATE",
            )
            .bind(project_id)
            .bind(repo)
            .bind(pr_number as i64)
            .fetch_optional(&mut **tx)
            .await?
        } else {
            sqlx::query_as(
                "SELECT id, data FROM issue_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND data::jsonb->>'repo' IS NULL
                   AND (data::jsonb->>'pr_number')::bigint = $2
                 ORDER BY updated_at DESC
                 LIMIT 1
                 FOR UPDATE",
            )
            .bind(project_id)
            .bind(pr_number as i64)
            .fetch_optional(&mut **tx)
            .await?
        };
        match row {
            Some((id, data)) => {
                let workflow = serde_json::from_str(&data)?;
                Ok(Some((id, workflow)))
            }
            None => Ok(None),
        }
    }

    /// List all workflow rows whose `project_id` contains `/workspaces/`,
    /// indicating a corrupt worktree path was stored instead of the canonical root.
    pub async fn list_with_worktree_project_ids(
        &self,
    ) -> anyhow::Result<Vec<IssueWorkflowInstance>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data FROM issue_workflows
             WHERE data::jsonb->>'project_id' LIKE '%/workspaces/%'
             ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    /// Replace a workflow row's `project_id` with `new_project_id`, rekeying the
    /// primary key (which embeds the project_id). Old row is deleted; new row is inserted.
    pub async fn repair_project_id(
        &self,
        old_row_id: &str,
        new_project_id: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let old_workflow = self
            .load_for_update_by_id(&mut tx, old_row_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("workflow row '{old_row_id}' not found"))?;

        let mut new_workflow = old_workflow.clone();
        new_workflow.project_id = new_project_id.to_string();
        new_workflow.id = workflow_id(
            &new_workflow.project_id,
            new_workflow.repo.as_deref(),
            new_workflow.issue_number,
        );

        if new_workflow.id == old_row_id {
            return Ok(());
        }

        new_workflow.updated_at = chrono::Utc::now();

        let new_data = serde_json::to_string(&new_workflow)?;
        sqlx::query(
            "INSERT INTO issue_workflows (id, data, created_at) VALUES ($1, $2, $3)
             ON CONFLICT(id) DO UPDATE SET data = EXCLUDED.data,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&new_workflow.id)
        .bind(&new_data)
        .bind(old_workflow.created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM issue_workflows WHERE id = $1")
            .bind(old_row_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        tracing::info!(
            old_row_id,
            old_project_id = %old_workflow.project_id,
            new_project_id,
            new_row_id = %new_workflow.id,
            "repaired corrupt workflow project_id"
        );
        Ok(())
    }

    /// Mark a workflow row as `Failed` with the given reason detail.
    pub async fn mark_workflow_failed_with_reason(
        &self,
        row_id: &str,
        reason: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let Some(mut workflow) = self.load_for_update_by_id(&mut tx, row_id).await? else {
            return Ok(());
        };
        workflow.apply_event(
            IssueLifecycleEvent::new(IssueLifecycleEventKind::WorkflowFailed)
                .with_detail(reason.to_string()),
        );
        self.upsert_in_tx(&mut tx, &workflow).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn upsert_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        workflow: &IssueWorkflowInstance,
    ) -> anyhow::Result<()> {
        let data = serde_json::to_string(workflow)?;
        sqlx::query(
            "UPDATE issue_workflows
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
            .get_by_issue(project_id, Some("owner/repo"), 882)
            .await?
            .expect("workflow by issue");
        let by_pr = store
            .get_by_pr(project_id, Some("owner/repo"), 909)
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

    #[tokio::test]
    async fn issue_workflow_store_scopes_identity_by_repo() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let project_id = "/tmp/shared-project";
        store
            .record_issue_scheduled(project_id, Some("owner/repo-a"), 42, "task-a", &[], false)
            .await?;
        store
            .record_issue_scheduled(project_id, Some("owner/repo-b"), 42, "task-b", &[], false)
            .await?;

        let a = store
            .get_by_issue(project_id, Some("owner/repo-a"), 42)
            .await?
            .expect("repo-a workflow");
        let b = store
            .get_by_issue(project_id, Some("owner/repo-b"), 42)
            .await?
            .expect("repo-b workflow");

        assert_ne!(a.id, b.id);
        Ok(())
    }

    #[tokio::test]
    async fn issue_workflow_store_records_cancelled_pr_tasks_as_cancelled() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let project_id = "/tmp/project-cancel";
        store
            .record_issue_scheduled(project_id, Some("owner/repo"), 7, "task-1", &[], false)
            .await?;
        store
            .record_pr_detected(
                project_id,
                Some("owner/repo"),
                7,
                "task-1",
                55,
                "https://github.com/owner/repo/pull/55",
            )
            .await?;
        let workflow = store
            .record_terminal_for_pr(project_id, Some("owner/repo"), 55, false, true, None)
            .await?
            .expect("workflow");
        assert_eq!(workflow.state, IssueLifecycleState::Cancelled);
        Ok(())
    }

    #[tokio::test]
    async fn claim_feedback_candidates_reclaims_stale_claims() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let project_id = "/tmp/project-stale-claim";
        store
            .record_issue_scheduled(project_id, Some("owner/repo"), 9, "task-1", &[], false)
            .await?;
        store
            .record_pr_detected(
                project_id,
                Some("owner/repo"),
                9,
                "task-1",
                77,
                "https://github.com/owner/repo/pull/77",
            )
            .await?;
        store
            .record_feedback_task_scheduled(project_id, Some("owner/repo"), 77, "task-2")
            .await?;

        let mut workflow = store
            .get_by_pr(project_id, Some("owner/repo"), 77)
            .await?
            .expect("workflow");
        workflow.feedback_claimed_at = Some(Utc::now() - chrono::Duration::minutes(10));
        store.upsert(&workflow).await?;
        sqlx::query(
            "UPDATE issue_workflows
             SET updated_at = CURRENT_TIMESTAMP - INTERVAL '10 minutes'
             WHERE id = $1",
        )
        .bind(&workflow.id)
        .execute(&store.pool)
        .await?;

        let claimed = store
            .claim_feedback_candidates(16, Utc::now() - chrono::Duration::minutes(5))
            .await?;
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].pr_number, Some(77));
        assert_eq!(claimed[0].state, IssueLifecycleState::AddressingFeedback);
        Ok(())
    }

    #[tokio::test]
    async fn claim_feedback_candidates_skips_malformed_rows() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let project_id = "/tmp/project-malformed";
        store
            .record_issue_scheduled(project_id, Some("owner/repo"), 10, "task-1", &[], false)
            .await?;
        store
            .record_pr_detected(
                project_id,
                Some("owner/repo"),
                10,
                "task-1",
                88,
                "https://github.com/owner/repo/pull/88",
            )
            .await?;
        sqlx::query(
            "INSERT INTO issue_workflows (id, data) VALUES ($1, $2)
             ON CONFLICT (id) DO UPDATE SET data = EXCLUDED.data",
        )
        .bind("malformed-workflow")
        .bind(r#"{"project_id":"/tmp/project-malformed","repo":"owner/repo","state":"pr_open","pr_number":999}"#)
        .execute(&store.pool)
        .await?;

        let claimed = store
            .claim_feedback_candidates(16, Utc::now() - chrono::Duration::minutes(5))
            .await?;
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].pr_number, Some(88));
        Ok(())
    }

    #[tokio::test]
    async fn update_project_path_patches_project_id_field() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let old_project_id = "/tmp/old-project-path-test";
        store
            .record_issue_scheduled(
                old_project_id,
                Some("owner/repo"),
                101,
                "task-path-1",
                &[],
                false,
            )
            .await?;
        let before = store
            .get_by_issue(old_project_id, Some("owner/repo"), 101)
            .await?
            .expect("workflow before update");
        let before_state = before.state;

        store
            .update_project_path(&before.id, "/tmp/new-project-path-test")
            .await?;

        let after = store
            .get_by_issue("/tmp/new-project-path-test", Some("owner/repo"), 101)
            .await?
            .expect("workflow after update");
        assert_eq!(after.project_id, "/tmp/new-project-path-test");
        assert_eq!(after.state, before_state);
        Ok(())
    }

    #[tokio::test]
    async fn update_project_path_noop_on_missing_id() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        // Must not return an error when no row matches
        store
            .update_project_path("nonexistent-workflow-id", "/tmp/any-path")
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn repair_project_id_rekeyes_row() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let corrupt_id = "/data/workspaces/abc-uuid-repair-test";
        store
            .record_issue_scheduled(corrupt_id, Some("owner/repo"), 9001, "task-r1", &[], false)
            .await?;

        let workflow = store
            .get_by_issue(corrupt_id, Some("owner/repo"), 9001)
            .await?
            .expect("row should exist");
        let old_row_id = workflow.id.clone();

        let canonical = "/real/canonical/root";
        store.repair_project_id(&old_row_id, canonical).await?;

        // Old row gone, new row present with correct project_id
        let old = store
            .get_by_issue(corrupt_id, Some("owner/repo"), 9001)
            .await?;
        assert!(old.is_none(), "old row should be removed");

        let new = store
            .get_by_issue(canonical, Some("owner/repo"), 9001)
            .await?
            .expect("new row should exist");
        assert_eq!(new.project_id, canonical);
        Ok(())
    }

    #[tokio::test]
    async fn mark_workflow_failed_with_reason_sets_state() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let project_id = "/tmp/project-mark-failed-test";
        store
            .record_issue_scheduled(project_id, Some("owner/repo"), 9002, "task-mf1", &[], false)
            .await?;

        let workflow = store
            .get_by_issue(project_id, Some("owner/repo"), 9002)
            .await?
            .expect("row should exist");

        store
            .mark_workflow_failed_with_reason(&workflow.id, "project root not found")
            .await?;

        let updated = store
            .get_by_issue(project_id, Some("owner/repo"), 9002)
            .await?
            .expect("row should still exist");
        assert_eq!(updated.state, IssueLifecycleState::Failed);
        assert!(
            updated
                .last_event
                .as_ref()
                .and_then(|e| e.detail.as_deref())
                == Some("project root not found"),
            "detail should be recorded"
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_with_worktree_project_ids_filters_correctly() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let corrupt = "/data/workspaces/xyz-uuid-list-test";
        let canonical = "/tmp/canonical-list-test";

        store
            .record_issue_scheduled(corrupt, Some("owner/repo"), 9003, "task-l1", &[], false)
            .await?;
        store
            .record_issue_scheduled(canonical, Some("owner/repo"), 9004, "task-l2", &[], false)
            .await?;

        let corrupt_rows = store.list_with_worktree_project_ids().await?;
        assert!(
            corrupt_rows.iter().any(|w| w.project_id == corrupt),
            "worktree row should appear"
        );
        assert!(
            !corrupt_rows.iter().any(|w| w.project_id == canonical),
            "canonical row should not appear"
        );
        Ok(())
    }
}
