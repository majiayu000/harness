use super::*;

async fn open_restart_suppression_store(
) -> anyhow::Result<Option<(tempfile::TempDir, WorkflowRuntimeStore)>> {
    let Ok(database_url) = resolve_database_url(None) else {
        return Ok(None);
    };
    let dir = tempfile::tempdir()?;
    let store =
        match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await {
            Ok(store) => store,
            Err(_) => return Ok(None),
        };
    Ok(Some((dir, store)))
}

#[tokio::test]
async fn retrying_child_and_unchanged_remote_fact_suppress_sweeps_after_restart(
) -> anyhow::Result<()> {
    let Some((dir, store)) = open_restart_suppression_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        123,
    );
    upsert_github_issue_pr_definition(&store).await?;
    let parent = issue_instance(
        workflow_id.clone(),
        project_root.to_string_lossy().into_owned(),
        Some("owner/repo".to_string()),
        123,
        "awaiting_feedback",
    )
    .with_data(json!({
        "project_id": project_root.to_string_lossy(),
        "repo": "owner/repo",
        "issue_number": 123,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "task-1",
    }));
    crate::test_helpers::force_upsert_runtime_instance_for_test(&store, &parent).await?;

    let mut child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-retrying-then-same-fact")
    .with_parent(workflow_id.clone());
    crate::test_helpers::force_upsert_runtime_instance_for_test(&store, &child).await?;
    let inspect =
        WorkflowCommand::enqueue_activity(PR_FEEDBACK_INSPECT_ACTIVITY, "inspect-pr-feedback-77");
    let inspect_id = store.enqueue_command(&child.id, None, &inspect).await?;
    let owner = "retrying-pr-feedback-child";
    let claim = store
        .claim_pending_commands(owner, chrono::Utc::now() + chrono::Duration::minutes(1), 10)
        .await?
        .into_iter()
        .find(|record| record.id == inspect_id)
        .expect("inspect command should be claimed");
    let deferred = store
        .defer_claimed_command_if_owned(
            &inspect_id,
            owner,
            claim.dispatch_claim_generation,
            DispatchBarrierInput::new(
                DispatchBarrierReasonCode::RuntimePolicyDisabled,
                "retry inspect later",
                "project",
            ),
            chrono::Utc::now(),
            DispatchBackoffPolicy::from_seconds(5, 20)?,
        )
        .await?;
    assert!(matches!(deferred, DeferClaimedCommandOutcome::Deferred(_)));
    let command_count = store.commands_for(&workflow_id).await?.len();
    assert_eq!(
        request_pr_feedback_sweep(&store, &workflow_id).await?,
        PrFeedbackSweepRequestOutcome::ActiveCommandExists {
            workflow_id: workflow_id.clone(),
            task_id: "task-1".to_string(),
        }
    );
    assert_eq!(store.commands_for(&workflow_id).await?.len(), command_count);

    store
        .mark_command_status(&inspect_id, WorkflowCommandStatus::Completed)
        .await?;
    let observed_fact_at = chrono::Utc::now() - chrono::Duration::hours(25);
    let snapshot = RemoteFactSnapshot::new(
        "github",
        "owner/repo",
        "pull_request",
        77,
        "waiting",
        json!({
            "head_oid": "same-head",
            "review_decision": "REVIEW_REQUIRED",
            "updated_at": observed_fact_at.to_rfc3339(),
        }),
        chrono::Utc::now(),
    );
    let fact_hash = snapshot.fact_hash.clone();
    store.upsert_remote_fact_snapshot(&snapshot).await?;
    child.state = "no_actionable_feedback".to_string();
    child.data = json!({
        "remote_fact_hash": fact_hash,
        "remote_fact_activity_at": observed_fact_at.to_rfc3339(),
    });
    crate::test_helpers::force_upsert_runtime_instance_for_test(&store, &child).await?;
    sqlx::query(
        "UPDATE workflow_instances
         SET updated_at = CURRENT_TIMESTAMP - INTERVAL '25 hours'
         WHERE id = $1",
    )
    .bind(&child.id)
    .execute(store.pool())
    .await?;
    drop(store);

    let database_url = resolve_database_url(None)?;
    let reopened =
        WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await?;
    let command_count = reopened.commands_for(&workflow_id).await?.len();
    assert_eq!(
        request_pr_feedback_sweep(&reopened, &workflow_id).await?,
        PrFeedbackSweepRequestOutcome::ActiveCommandExists {
            workflow_id: workflow_id.clone(),
            task_id: "task-1".to_string(),
        }
    );
    assert_eq!(
        reopened.commands_for(&workflow_id).await?.len(),
        command_count
    );
    Ok(())
}
