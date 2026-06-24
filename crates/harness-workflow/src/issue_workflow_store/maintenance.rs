use super::IssueWorkflowStore;
use crate::issue_lifecycle::{
    workflow_id, IssueLifecycleEvent, IssueLifecycleEventKind, IssueWorkflowInstance,
};

impl IssueWorkflowStore {
    /// List workflow rows whose `project_id` still points at an isolated worktree.
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

    /// Replace a workflow row's `project_id` and rekey the primary key.
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

        if let Some(existing_workflow) = self
            .load_for_update_by_id(&mut tx, &new_workflow.id)
            .await?
        {
            anyhow::bail!(
                "cannot repair workflow row '{old_row_id}' to canonical id '{}': canonical row already exists in state {:?} (updated_at {})",
                new_workflow.id,
                existing_workflow.state,
                existing_workflow.updated_at,
            );
        }

        new_workflow.updated_at = chrono::Utc::now();

        let new_data = serde_json::to_string(&new_workflow)?;
        let insert_result = sqlx::query(
            "INSERT INTO issue_workflows (id, data, created_at) VALUES ($1, $2, $3)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(&new_workflow.id)
        .bind(&new_data)
        .bind(old_workflow.created_at)
        .execute(&mut *tx)
        .await?;
        if insert_result.rows_affected() != 1 {
            anyhow::bail!(
                "cannot repair workflow row '{old_row_id}' to canonical id '{}': canonical row appeared during repair",
                new_workflow.id,
            );
        }

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
}
