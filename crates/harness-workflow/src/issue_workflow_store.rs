use chrono::{DateTime, Utc};
use harness_core::db::{
    pg_create_schema_if_not_exists, pg_open_pool, pg_open_pool_schematized, resolve_database_url,
    Migration, PgMigrator,
};
use sqlx::postgres::PgPool;
use sqlx::Postgres;
use std::path::Path;

use crate::issue_lifecycle::{
    legacy_schema_for_path, workflow_id, IssueLifecycleEvent, IssueLifecycleEventKind,
    IssueLifecycleState, IssueWorkflowInstance,
};

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

    /// Transition a `ReadyToMerge` workflow to `Done` after human approval.
    ///
    /// Returns `None` if no workflow is found for the given PR.  The state
    /// machine guard in `apply_event` silently discards the event when the
    /// workflow is not in `ReadyToMerge` (e.g. already `Done`).
    pub async fn record_merge_approved(
        &self,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_by_pr(project_id, repo, pr_number, |workflow| {
            workflow.apply_event(IssueLifecycleEvent::new(
                IssueLifecycleEventKind::HumanMergeApproved,
            ));
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
        let wf_id = workflow_id(project_id, repo, issue_number);
        let placeholder = IssueWorkflowInstance::new(
            project_id.to_string(),
            repo.map(|r| r.to_string()),
            issue_number,
        );
        let mut tx = self.pool.begin().await?;
        self.insert_placeholder(&mut tx, &wf_id, &placeholder)
            .await?;
        let mut workflow = self
            .load_for_update_by_id(&mut tx, &wf_id)
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
        let wf_id = workflow_id(project_id, repo, issue_number);
        let Some(mut workflow) = self.load_for_update_by_id(&mut tx, &wf_id).await? else {
            return Ok(None);
        };
        f(&mut workflow);
        self.upsert_in_tx(&mut tx, &workflow).await?;
        tx.commit().await?;
        Ok(Some(workflow))
    }

    pub(crate) async fn update_by_pr<F>(
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
        let Some((wf_id, mut workflow)) = self
            .load_for_update_by_pr(&mut tx, project_id, repo, pr_number)
            .await?
        else {
            return Ok(None);
        };
        f(&mut workflow);
        self.upsert_in_tx(&mut tx, &workflow).await?;
        debug_assert_eq!(workflow.id, wf_id);
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
#[path = "issue_workflow_store_tests.rs"]
mod tests;
