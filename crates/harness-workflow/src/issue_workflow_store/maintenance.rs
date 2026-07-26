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
        )?;
        self.upsert_in_tx(&mut tx, &workflow).await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::IssueWorkflowStore;
    use crate::issue_lifecycle::IssueLifecycleState;

    async fn open_test_store() -> anyhow::Result<Option<IssueWorkflowStore>> {
        if std::env::var("DATABASE_URL").is_err() {
            return Ok(None);
        }
        let dir = tempfile::tempdir()?;
        match IssueWorkflowStore::open(&dir.path().join("issue_workflows.db")).await {
            Ok(store) => Ok(Some(store)),
            Err(_) => Ok(None),
        }
    }

    #[tokio::test]
    async fn repair_project_id_rekeys_row() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let corrupt = "/data/workspaces/abc-uuid-repair-test";
        store
            .record_issue_scheduled(corrupt, Some("owner/repo"), 9001, "task-r1", &[], false)
            .await?;
        let old = store
            .get_by_issue(corrupt, Some("owner/repo"), 9001)
            .await?
            .expect("row");
        store
            .repair_project_id(&old.id, "/real/canonical/root")
            .await?;
        assert!(store
            .get_by_issue(corrupt, Some("owner/repo"), 9001)
            .await?
            .is_none());
        assert_eq!(
            store
                .get_by_issue("/real/canonical/root", Some("owner/repo"), 9001)
                .await?
                .expect("canonical row")
                .project_id,
            "/real/canonical/root"
        );
        Ok(())
    }

    #[tokio::test]
    async fn repair_project_id_refuses_to_overwrite_existing_canonical_row() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let corrupt = "/data/workspaces/abc-uuid-conflict-test";
        let canonical = "/real/canonical/conflict-root";
        store
            .record_issue_scheduled(corrupt, Some("owner/repo"), 9005, "corrupt", &[], false)
            .await?;
        store
            .record_issue_scheduled(canonical, Some("owner/repo"), 9005, "canonical", &[], false)
            .await?;
        let corrupt_before = store
            .get_by_issue(corrupt, Some("owner/repo"), 9005)
            .await?
            .expect("corrupt row");
        let canonical_before = store
            .get_by_issue(canonical, Some("owner/repo"), 9005)
            .await?
            .expect("canonical row");
        let error = store
            .repair_project_id(&corrupt_before.id, canonical)
            .await
            .expect_err("conflict");
        assert!(error.to_string().contains("canonical row already exists"));
        assert_eq!(
            store
                .get_by_issue(corrupt, Some("owner/repo"), 9005)
                .await?
                .expect("corrupt row"),
            corrupt_before
        );
        assert_eq!(
            store
                .get_by_issue(canonical, Some("owner/repo"), 9005)
                .await?
                .expect("canonical row"),
            canonical_before
        );
        Ok(())
    }

    #[tokio::test]
    async fn mark_workflow_failed_with_reason_sets_state() -> anyhow::Result<()> {
        let Some(store) = open_test_store().await? else {
            return Ok(());
        };
        let project = "/tmp/project-mark-failed-test";
        let workflow = store
            .record_issue_scheduled(project, Some("owner/repo"), 9002, "task", &[], false)
            .await?;
        store
            .mark_workflow_failed_with_reason(&workflow.id, "project root not found")
            .await?;
        let updated = store
            .get_by_issue(project, Some("owner/repo"), 9002)
            .await?
            .expect("row");
        assert_eq!(updated.state, IssueLifecycleState::Failed);
        assert_eq!(
            updated.last_event.and_then(|event| event.detail).as_deref(),
            Some("project root not found")
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
            .record_issue_scheduled(corrupt, Some("owner/repo"), 9003, "t1", &[], false)
            .await?;
        store
            .record_issue_scheduled(canonical, Some("owner/repo"), 9004, "t2", &[], false)
            .await?;
        let rows = store.list_with_worktree_project_ids().await?;
        assert!(rows.iter().any(|workflow| workflow.project_id == corrupt));
        assert!(!rows.iter().any(|workflow| workflow.project_id == canonical));
        Ok(())
    }
}
