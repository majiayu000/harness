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
