use super::IssueWorkflowStore;
use crate::issue_lifecycle::IssueLifecycleState;
use chrono::Utc;

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
async fn record_merge_approved_transitions_workflow_to_done() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/project-merge-approved";
    store
        .record_issue_scheduled(project_id, Some("owner/repo"), 20, "task-1", &[], false)
        .await?;
    store
        .record_pr_detected(
            project_id,
            Some("owner/repo"),
            20,
            "task-1",
            120,
            "https://github.com/owner/repo/pull/120",
        )
        .await?;
    store
        .record_terminal_for_pr(project_id, Some("owner/repo"), 120, true, false, None)
        .await?;

    let workflow = store
        .get_by_pr(project_id, Some("owner/repo"), 120)
        .await?
        .expect("workflow should be ready_to_merge");
    assert_eq!(workflow.state, IssueLifecycleState::ReadyToMerge);

    let updated = store
        .record_merge_approved(project_id, Some("owner/repo"), 120)
        .await?
        .expect("updated workflow");
    assert_eq!(updated.state, IssueLifecycleState::Done);
    Ok(())
}

#[tokio::test]
async fn repair_project_id_rekeys_row() -> anyhow::Result<()> {
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
async fn repair_project_id_refuses_to_overwrite_existing_canonical_row() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let corrupt_project_id = "/data/workspaces/abc-uuid-conflict-test";
    let canonical_project_id = "/real/canonical/conflict-root";

    store
        .record_issue_scheduled(
            corrupt_project_id,
            Some("owner/repo"),
            9005,
            "task-corrupt",
            &[],
            false,
        )
        .await?;
    store
        .record_issue_scheduled(
            canonical_project_id,
            Some("owner/repo"),
            9005,
            "task-canonical",
            &[],
            false,
        )
        .await?;
    store
        .record_pr_detected(
            canonical_project_id,
            Some("owner/repo"),
            9005,
            "task-canonical",
            45,
            "https://github.com/owner/repo/pull/45",
        )
        .await?;

    let corrupt_before = store
        .get_by_issue(corrupt_project_id, Some("owner/repo"), 9005)
        .await?
        .expect("corrupt row should exist");
    let canonical_before = store
        .get_by_issue(canonical_project_id, Some("owner/repo"), 9005)
        .await?
        .expect("canonical row should exist");

    let err = store
        .repair_project_id(&corrupt_before.id, canonical_project_id)
        .await
        .expect_err("repair should fail when canonical row already exists");
    assert!(
        err.to_string().contains("canonical row already exists"),
        "unexpected error: {err}",
    );

    let corrupt_after = store
        .get_by_issue(corrupt_project_id, Some("owner/repo"), 9005)
        .await?
        .expect("corrupt row should remain for manual remediation");
    let canonical_after = store
        .get_by_issue(canonical_project_id, Some("owner/repo"), 9005)
        .await?
        .expect("canonical row should remain unchanged");

    assert_eq!(store.row_count().await?, 2);
    assert_eq!(corrupt_after, corrupt_before);
    assert_eq!(canonical_after, canonical_before);
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
    assert_eq!(
        updated
            .last_event
            .as_ref()
            .and_then(|e| e.detail.as_deref()),
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
