use sqlx::Postgres;

use super::IssueWorkflowStore;
use crate::issue_lifecycle::IssueWorkflowInstance;

impl IssueWorkflowStore {
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
                   AND LOWER(data::jsonb->>'repo') = LOWER($2)
                   AND (data::jsonb->>'issue_number')::bigint = $3
                 ORDER BY (data::jsonb->>'repo' = $2) DESC, updated_at DESC
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
                   AND LOWER(data::jsonb->>'repo') = LOWER($2)
                   AND (data::jsonb->>'pr_number')::bigint = $3
                 ORDER BY (data::jsonb->>'repo' = $2) DESC, updated_at DESC
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

    pub(super) async fn load_for_update_by_pr(
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
                   AND LOWER(data::jsonb->>'repo') = LOWER($2)
                   AND (data::jsonb->>'pr_number')::bigint = $3
                 ORDER BY (data::jsonb->>'repo' = $2) DESC, updated_at DESC
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
        row.map(|(id, data)| Ok((id, serde_json::from_str(&data)?)))
            .transpose()
    }

    pub(super) async fn load_for_update_by_issue(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
    ) -> anyhow::Result<Option<(String, IssueWorkflowInstance)>> {
        let row: Option<(String, String)> = if let Some(repo) = repo {
            sqlx::query_as(
                "SELECT id, data FROM issue_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND LOWER(data::jsonb->>'repo') = LOWER($2)
                   AND (data::jsonb->>'issue_number')::bigint = $3
                 ORDER BY (data::jsonb->>'repo' = $2) DESC, updated_at DESC
                 LIMIT 1
                 FOR UPDATE",
            )
            .bind(project_id)
            .bind(repo)
            .bind(issue_number as i64)
            .fetch_optional(&mut **tx)
            .await?
        } else {
            sqlx::query_as(
                "SELECT id, data FROM issue_workflows
                 WHERE data::jsonb->>'project_id' = $1
                   AND data::jsonb->>'repo' IS NULL
                   AND (data::jsonb->>'issue_number')::bigint = $2
                 ORDER BY updated_at DESC
                 LIMIT 1
                 FOR UPDATE",
            )
            .bind(project_id)
            .bind(issue_number as i64)
            .fetch_optional(&mut **tx)
            .await?
        };
        row.map(|(id, data)| Ok((id, serde_json::from_str(&data)?)))
            .transpose()
    }
}
