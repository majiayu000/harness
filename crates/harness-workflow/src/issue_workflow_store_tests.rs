use super::{IssueMergeApprovalOutcome, IssueWorkflowStore};
use crate::issue_lifecycle::{
    IssueLifecycleEvent, IssueLifecycleEventKind, IssueLifecycleState, ReviewFallbackSnapshot,
    ReviewFallbackTier, ReviewFallbackTrigger,
};
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
async fn claim_feedback_candidates_does_not_reclaim_live_addressing_feedback_tasks(
) -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/project-live-feedback";
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

    let workflow = store
        .get_by_pr(project_id, Some("owner/repo"), 77)
        .await?
        .expect("workflow");
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
    assert!(claimed.is_empty());

    let persisted = store
        .get_by_pr(project_id, Some("owner/repo"), 77)
        .await?
        .expect("workflow");
    assert_eq!(persisted.state, IssueLifecycleState::AddressingFeedback);
    assert_eq!(persisted.active_task_id.as_deref(), Some("task-2"));
    Ok(())
}

#[tokio::test]
async fn claim_feedback_candidates_reclaims_stale_claim_placeholders() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/project-stale-claim";
    store
        .record_issue_scheduled(project_id, Some("owner/repo"), 10, "task-1", &[], false)
        .await?;
    store
        .record_pr_detected(
            project_id,
            Some("owner/repo"),
            10,
            "task-1",
            78,
            "https://github.com/owner/repo/pull/78",
        )
        .await?;

    let claimed = store
        .claim_feedback_candidates(16, Utc::now() - chrono::Duration::minutes(5))
        .await?;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].pr_number, Some(78));
    assert_eq!(claimed[0].state, IssueLifecycleState::FeedbackClaimed);
    assert!(claimed[0].active_task_id.is_none());

    sqlx::query(
        "UPDATE issue_workflows
         SET updated_at = CURRENT_TIMESTAMP - INTERVAL '10 minutes'
         WHERE id = $1",
    )
    .bind(&claimed[0].id)
    .execute(&store.pool)
    .await?;

    let reclaimed = store
        .claim_feedback_candidates(16, Utc::now() - chrono::Duration::minutes(5))
        .await?;
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].pr_number, Some(78));
    assert_eq!(reclaimed[0].state, IssueLifecycleState::FeedbackClaimed);
    assert!(reclaimed[0].active_task_id.is_none());
    Ok(())
}

#[tokio::test]
async fn claim_feedback_candidates_reclaims_legacy_addressing_feedback_placeholders(
) -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/project-legacy-claim";
    store
        .record_issue_scheduled(project_id, Some("owner/repo"), 12, "task-1", &[], false)
        .await?;
    store
        .record_pr_detected(
            project_id,
            Some("owner/repo"),
            12,
            "task-1",
            80,
            "https://github.com/owner/repo/pull/80",
        )
        .await?;
    store
        .record_feedback_task_scheduled(
            project_id,
            Some("owner/repo"),
            80,
            "claim:legacy-workflow-80",
        )
        .await?;

    let workflow = store
        .get_by_pr(project_id, Some("owner/repo"), 80)
        .await?
        .expect("workflow");
    assert_eq!(workflow.state, IssueLifecycleState::AddressingFeedback);
    assert_eq!(
        workflow.active_task_id.as_deref(),
        Some("claim:legacy-workflow-80")
    );
    sqlx::query(
        "UPDATE issue_workflows
         SET updated_at = CURRENT_TIMESTAMP - INTERVAL '10 minutes'
         WHERE id = $1",
    )
    .bind(&workflow.id)
    .execute(&store.pool)
    .await?;

    let reclaimed = store
        .claim_feedback_candidates(16, Utc::now() - chrono::Duration::minutes(5))
        .await?;
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].pr_number, Some(80));
    assert_eq!(reclaimed[0].state, IssueLifecycleState::FeedbackClaimed);
    assert!(reclaimed[0].active_task_id.is_none());
    assert!(reclaimed[0].feedback_claimed_at.is_some());
    Ok(())
}

#[tokio::test]
async fn bind_feedback_task_if_claimed_promotes_placeholder_to_active_feedback(
) -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/project-bind-feedback";
    store
        .record_issue_scheduled(project_id, Some("owner/repo"), 11, "task-1", &[], false)
        .await?;
    store
        .record_pr_detected(
            project_id,
            Some("owner/repo"),
            11,
            "task-1",
            79,
            "https://github.com/owner/repo/pull/79",
        )
        .await?;
    let claimed = store
        .claim_feedback_candidates(16, Utc::now() - chrono::Duration::minutes(5))
        .await?;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].state, IssueLifecycleState::FeedbackClaimed);

    let workflow = store
        .bind_feedback_task_if_claimed(project_id, Some("owner/repo"), 79, "task-2")
        .await?
        .expect("workflow");
    assert_eq!(workflow.state, IssueLifecycleState::AddressingFeedback);
    assert_eq!(workflow.active_task_id.as_deref(), Some("task-2"));
    assert!(workflow.feedback_claimed_at.is_none());

    let persisted = store
        .get_by_pr(project_id, Some("owner/repo"), 79)
        .await?
        .expect("workflow");
    assert_eq!(persisted.state, IssueLifecycleState::AddressingFeedback);
    assert_eq!(persisted.active_task_id.as_deref(), Some("task-2"));
    Ok(())
}

#[tokio::test]
async fn release_feedback_claim_returns_placeholder_to_awaiting_feedback() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/project-release-feedback";
    store
        .record_issue_scheduled(project_id, Some("owner/repo"), 12, "task-1", &[], false)
        .await?;
    store
        .record_pr_detected(
            project_id,
            Some("owner/repo"),
            12,
            "task-1",
            80,
            "https://github.com/owner/repo/pull/80",
        )
        .await?;
    let claimed = store
        .claim_feedback_candidates(16, Utc::now() - chrono::Duration::minutes(5))
        .await?;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].state, IssueLifecycleState::FeedbackClaimed);

    let workflow = store
        .release_feedback_claim(project_id, Some("owner/repo"), 80, "enqueue failed")
        .await?
        .expect("workflow");
    assert_eq!(workflow.state, IssueLifecycleState::AwaitingFeedback);
    assert!(workflow.active_task_id.is_none());
    assert!(workflow.feedback_claimed_at.is_none());
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
async fn repeated_merge_approval_from_done_is_applied_idempotently() -> anyhow::Result<()> {
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

    let updated = match store
        .record_merge_approved(project_id, Some("owner/repo"), 120)
        .await?
    {
        IssueMergeApprovalOutcome::Applied(workflow) => workflow,
        other => panic!("expected applied approval outcome, got {other:?}"),
    };
    assert_eq!(updated.state, IssueLifecycleState::Done);
    let repeated = store
        .record_merge_approved(project_id, Some("owner/repo"), 120)
        .await?;
    let IssueMergeApprovalOutcome::Applied(repeated) = repeated else {
        panic!("expected repeated approval to be applied, got {repeated:?}");
    };
    assert_eq!(repeated.state, IssueLifecycleState::Done);
    assert_eq!(
        repeated.last_event.map(|event| event.kind),
        Some(IssueLifecycleEventKind::HumanMergeApproved)
    );
    Ok(())
}

#[tokio::test]
async fn merge_approval_wrong_state_returns_transition_error() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/project-merge-approved-wrong-state";
    store
        .record_issue_scheduled(project_id, Some("owner/repo"), 22, "task-1", &[], false)
        .await?;
    store
        .record_pr_detected(
            project_id,
            Some("owner/repo"),
            22,
            "task-1",
            121,
            "https://github.com/owner/repo/pull/121",
        )
        .await?;
    let before = store
        .get_by_pr(project_id, Some("owner/repo"), 121)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workflow should exist"))?;

    let error = store
        .record_merge_approved(project_id, Some("owner/repo"), 121)
        .await
        .expect_err("PrOpen merge approval must fail");
    assert!(error.to_string().contains("transition_not_allowed"));
    let after = store
        .get_by_pr(project_id, Some("owner/repo"), 121)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workflow should still exist"))?;
    assert_eq!(after, before);
    Ok(())
}

#[tokio::test]
async fn issue_workflow_store_reports_illegal_transition() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project = "/tmp/project-illegal-transition";
    seed_pr(&store, project, 23, 123).await?;
    store
        .record_pr_merged(project, Some("owner/repo"), 123, None)
        .await?;
    let error = store
        .record_implement_started(project, Some("owner/repo"), 23, "late-task")
        .await
        .expect_err("terminal transition must reach the caller");
    assert!(error.to_string().contains("transition_not_allowed"));
    Ok(())
}

#[tokio::test]
async fn record_ready_to_merge_with_fallback_persists_snapshot() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/project-ready-fallback";
    store
        .record_issue_scheduled(project_id, Some("owner/repo"), 21, "task-1", &[], false)
        .await?;
    store
        .record_pr_detected(
            project_id,
            Some("owner/repo"),
            21,
            "task-1",
            121,
            "https://github.com/owner/repo/pull/121",
        )
        .await?;
    let activated_at = Utc::now();
    let workflow = store
        .record_ready_to_merge_with_fallback(
            project_id,
            Some("owner/repo"),
            121,
            Some("fallback via silence"),
            ReviewFallbackSnapshot {
                tier: ReviewFallbackTier::C,
                trigger: ReviewFallbackTrigger::Silence,
                active_bot: Some("codex".to_string()),
                activated_at,
            },
        )
        .await?
        .expect("workflow");

    assert_eq!(workflow.state, IssueLifecycleState::ReadyToMerge);
    assert_eq!(
        workflow.review_fallback,
        Some(ReviewFallbackSnapshot {
            tier: ReviewFallbackTier::C,
            trigger: ReviewFallbackTrigger::Silence,
            active_bot: Some("codex".to_string()),
            activated_at,
        })
    );
    assert_eq!(
        workflow.last_event.and_then(|event| event.detail),
        Some("fallback via silence".to_string())
    );
    let retried = store
        .record_ready_to_merge_with_fallback(
            project_id,
            Some("owner/repo"),
            121,
            Some("retry"),
            ReviewFallbackSnapshot {
                tier: ReviewFallbackTier::C,
                trigger: ReviewFallbackTrigger::Silence,
                active_bot: Some("codex".to_string()),
                activated_at: Utc::now(),
            },
        )
        .await?
        .expect("workflow");
    assert_eq!(
        retried.review_fallback.expect("fallback").activated_at,
        activated_at
    );
    store
        .record_ready_to_merge_with_fallback(
            project_id,
            Some("owner/repo"),
            121,
            Some("conflict"),
            ReviewFallbackSnapshot {
                tier: ReviewFallbackTier::C,
                trigger: ReviewFallbackTrigger::GeminiQuota,
                active_bot: Some("codex".to_string()),
                activated_at: Utc::now(),
            },
        )
        .await
        .expect_err("different fallback identity must fail");
    Ok(())
}

async fn required_test_store() -> anyhow::Result<IssueWorkflowStore> {
    let configured = std::env::var("HARNESS_DATABASE_URL").map_err(|_| {
        anyhow::anyhow!(
            "GH1715 persistence tests require HARNESS_DATABASE_URL for an isolated disposable database"
        )
    })?;
    let database_url = harness_core::db::resolve_test_database_url(Some(&configured))?;
    let dir = tempfile::tempdir()?;
    IssueWorkflowStore::open_with_database_url(
        &dir.path().join("gh1715-issue-workflows.db"),
        Some(&database_url),
    )
    .await
}

async fn seed_pr(
    store: &IssueWorkflowStore,
    project: &str,
    issue: u64,
    pr: u64,
) -> anyhow::Result<()> {
    store
        .record_issue_scheduled(project, Some("owner/repo"), issue, "task-1", &[], false)
        .await?;
    store
        .record_pr_detected(
            project,
            Some("owner/repo"),
            issue,
            "task-1",
            pr,
            &format!("https://github.com/owner/repo/pull/{pr}"),
        )
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated HARNESS_DATABASE_URL"]
async fn issue_workflow_store_metadata_requires_valid_transition() -> anyhow::Result<()> {
    let store = required_test_store().await?;
    let project = "/tmp/gh1715-metadata";
    seed_pr(&store, project, 101, 201).await?;
    store
        .record_pr_merged(project, Some("owner/repo"), 201, None)
        .await?;
    let before = store
        .get_by_issue(project, Some("owner/repo"), 101)
        .await?
        .expect("workflow");
    store
        .record_issue_scheduled(
            project,
            Some("owner/repo"),
            101,
            "other",
            &["changed".into()],
            true,
        )
        .await
        .expect_err("terminal scheduling must fail");
    let after = store
        .get_by_issue(project, Some("owner/repo"), 101)
        .await?
        .expect("workflow");
    assert_eq!(after, before);
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated HARNESS_DATABASE_URL"]
async fn rejected_issue_lifecycle_store_update_rolls_back() -> anyhow::Result<()> {
    let store = required_test_store().await?;
    let project = "/tmp/gh1715-rollback";
    seed_pr(&store, project, 102, 202).await?;
    store
        .record_pr_merged(project, Some("owner/repo"), 202, None)
        .await?;
    let before = store
        .get_by_issue(project, Some("owner/repo"), 102)
        .await?
        .expect("workflow");
    store
        .record_implement_started(project, Some("owner/repo"), 102, "late-task")
        .await
        .expect_err("terminal implementation must fail");
    assert_eq!(
        store
            .get_by_issue(project, Some("owner/repo"), 102)
            .await?
            .expect("workflow"),
        before
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated HARNESS_DATABASE_URL"]
async fn feedback_claim_batch_aborts_on_illegal_transition() -> anyhow::Result<()> {
    let store = required_test_store().await?;
    let project = "/tmp/gh1715-batch";
    seed_pr(&store, project, 103, 203).await?;
    seed_pr(&store, project, 104, 204).await?;
    let first_before = store
        .get_by_pr(project, Some("owner/repo"), 203)
        .await?
        .expect("first");
    let result: anyhow::Result<()> = async {
        let mut tx = store.pool.begin().await?;
        let (_, mut first) = store
            .load_for_update_by_pr(&mut tx, project, Some("owner/repo"), 203)
            .await?
            .expect("first");
        first.apply_event(IssueLifecycleEvent::new(
            IssueLifecycleEventKind::FeedbackFound,
        ))?;
        store.upsert_in_tx(&mut tx, &first).await?;
        let (_, mut illegal) = store
            .load_for_update_by_pr(&mut tx, project, Some("owner/repo"), 204)
            .await?
            .expect("second");
        illegal.apply_event(IssueLifecycleEvent::new(
            IssueLifecycleEventKind::IssueScheduled,
        ))?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    assert!(result.is_err());
    assert_eq!(
        store
            .get_by_pr(project, Some("owner/repo"), 203)
            .await?
            .expect("first"),
        first_before
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated HARNESS_DATABASE_URL"]
async fn concurrent_valid_and_invalid_issue_transitions_preserve_winner() -> anyhow::Result<()> {
    let store = std::sync::Arc::new(required_test_store().await?);
    let project = "/tmp/gh1715-race";
    seed_pr(&store, project, 105, 205).await?;
    let valid_store = store.clone();
    let invalid_store = store.clone();
    let (valid, invalid) = tokio::join!(
        valid_store.record_pr_merged(project, Some("owner/repo"), 205, None),
        invalid_store.record_issue_scheduled(
            project,
            Some("owner/repo"),
            105,
            "late-task",
            &[],
            false
        )
    );
    assert!(valid.is_ok());
    assert!(invalid.is_err());
    assert_eq!(
        store
            .get_by_issue(project, Some("owner/repo"), 105)
            .await?
            .expect("workflow")
            .state,
        IssueLifecycleState::Done
    );
    Ok(())
}
