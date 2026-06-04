use super::*;

#[tokio::test]
async fn completed_inspecting_child_does_not_block_next_feedback_sweep() -> anyhow::Result<()> {
    let Ok(database_url) = resolve_database_url(None) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let store =
        match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await {
            Ok(store) => store,
            Err(_) => return Ok(()),
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
    );
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-completed")
    .with_parent(workflow_id.clone());
    store.upsert_instance(&child).await?;

    assert!(
        !has_active_pr_feedback_command(
            &store,
            &workflow_id,
            DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS,
        )
        .await?,
        "an inspecting child with no active command should not suppress future sweeps"
    );

    let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "inspect-pr-feedback-77",
    );
    store.enqueue_command(&child.id, None, &command).await?;
    let claimed = store
        .claim_pending_commands(
            "dispatching-child-pr-feedback-test",
            chrono::Utc::now() + chrono::Duration::seconds(30),
            1,
        )
        .await?;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].status, "dispatching");

    assert!(
        has_active_pr_feedback_command(
            &store,
            &workflow_id,
            DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS,
        )
        .await?,
        "an inspecting child with a pending command should still suppress duplicate sweeps"
    );
    Ok(())
}

#[tokio::test]
async fn failed_pr_feedback_child_suppresses_duplicate_feedback_sweep() -> anyhow::Result<()> {
    let Ok(database_url) = resolve_database_url(None) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let store =
        match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await {
            Ok(store) => store,
            Err(_) => return Ok(()),
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
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "failed",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-failed")
    .with_parent(workflow_id.clone());
    store.upsert_instance(&child).await?;
    let child_command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "inspect-pr-feedback-77",
    );
    let child_command_id = store
        .enqueue_command(&child.id, None, &child_command)
        .await?;
    store
        .mark_command_status(&child_command_id, "failed")
        .await?;

    assert!(
        has_active_pr_feedback_command(
            &store,
            &workflow_id,
            DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS,
        )
        .await?,
        "a recently failed inspection child should suppress duplicate sweeps"
    );

    let outcome = request_pr_feedback_sweep(&store, &workflow_id).await?;
    assert_eq!(
        outcome,
        PrFeedbackSweepRequestOutcome::ActiveCommandExists {
            workflow_id: workflow_id.clone(),
            task_id: "task-1".to_string(),
        }
    );
    assert!(
        store.commands_for(&workflow_id).await?.is_empty(),
        "the parent workflow must not enqueue another PR feedback child while the failed child is cooling down"
    );
    Ok(())
}

#[tokio::test]
async fn explicit_pr_feedback_request_starts_local_review_before_remote_suppression(
) -> anyhow::Result<()> {
    let Ok(database_url) = resolve_database_url(None) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let store =
        match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await {
            Ok(store) => store,
            Err(_) => return Ok(()),
        };
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let workflow_id = pr_workflow_id(&project_id, Some("owner/repo"), 77);
    upsert_github_issue_pr_definition(&store).await?;
    let parent = pr_scoped_instance(
        workflow_id.clone(),
        project_id,
        Some("owner/repo".to_string()),
        77,
        "awaiting_feedback",
    )
    .with_data(json!({
        "project_id": project_root.to_string_lossy(),
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "task-1",
    }));
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "failed",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-failed-explicit-request")
    .with_parent(workflow_id.clone());
    store.upsert_instance(&child).await?;
    let child_command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "inspect-pr-feedback-77",
    );
    let child_command_id = store
        .enqueue_command(&child.id, None, &child_command)
        .await?;
    store
        .mark_command_status(&child_command_id, "failed")
        .await?;

    let outcome = request_pr_feedback_sweep_for_pr(
        &store,
        PrFeedbackSweepRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            task_id: &TaskId::from_str("task-1"),
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
        },
    )
    .await?;

    assert_eq!(
        outcome,
        PrFeedbackSweepRequestOutcome::Requested {
            workflow_id: workflow_id.clone(),
            task_id: "task-1".to_string(),
        }
    );
    assert_eq!(
        store.commands_for(&workflow_id).await?.len(),
        1,
        "new explicit PR feedback activity should enqueue local review before remote feedback"
    );
    assert_eq!(
        store.commands_for(&workflow_id).await?[0]
            .command
            .activity_name(),
        Some(LOCAL_REVIEW_ACTIVITY)
    );
    Ok(())
}

#[tokio::test]
async fn failed_pr_feedback_child_respects_disabled_suppression_window() -> anyhow::Result<()> {
    let Ok(database_url) = resolve_database_url(None) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let store =
        match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await {
            Ok(store) => store,
            Err(_) => return Ok(()),
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
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "failed",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-failed")
    .with_parent(workflow_id.clone());
    store.upsert_instance(&child).await?;
    let child_command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "inspect-pr-feedback-77",
    );
    let child_command_id = store
        .enqueue_command(&child.id, None, &child_command)
        .await?;
    store
        .mark_command_status(&child_command_id, "failed")
        .await?;

    assert!(
        !has_active_pr_feedback_command(&store, &workflow_id, 0).await?,
        "a disabled failed-child suppression window should allow a fresh sweep"
    );

    let outcome =
        request_pr_feedback_sweep_with_failed_child_suppression_secs(&store, &workflow_id, 0)
            .await?;
    assert_eq!(
        outcome,
        PrFeedbackSweepRequestOutcome::Requested {
            workflow_id: workflow_id.clone(),
            task_id: "task-1".to_string(),
        }
    );
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    Ok(())
}

#[test]
fn failed_child_suppression_cutoff_handles_oversized_windows() {
    assert_eq!(failed_child_suppression_cutoff(0), None);

    let normal_cutoff = failed_child_suppression_cutoff(60)
        .expect("nonzero suppression window should produce a cutoff");
    assert!(normal_cutoff <= chrono::Utc::now());

    assert_eq!(
        failed_child_suppression_cutoff(u64::MAX),
        Some(chrono::DateTime::<chrono::Utc>::MIN_UTC)
    );
}

#[tokio::test]
async fn orphan_pending_child_does_not_block_next_feedback_sweep() -> anyhow::Result<()> {
    let Ok(database_url) = resolve_database_url(None) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let store =
        match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await {
            Ok(store) => store,
            Err(_) => return Ok(()),
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
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "pending",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-orphan")
    .with_parent(workflow_id.clone());
    store.upsert_instance(&child).await?;

    assert!(
        !has_active_pr_feedback_command(
            &store,
            &workflow_id,
            DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS,
        )
        .await?,
        "a pending child without an active inspection command should not suppress future sweeps"
    );

    let child_command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "inspect-pr-feedback-77",
    );
    let child_command_id = store
        .enqueue_command(&child.id, None, &child_command)
        .await?;

    assert!(
        has_active_pr_feedback_command(
            &store,
            &workflow_id,
            DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS,
        )
        .await?,
        "a pending child with an active inspection command should still suppress duplicate sweeps"
    );

    store
        .mark_command_status(&child_command_id, "completed")
        .await?;
    assert!(
        !has_active_pr_feedback_command(
            &store,
            &workflow_id,
            DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS,
        )
        .await?,
        "a pending child with no active inspection command should stop suppressing sweeps"
    );

    let outcome = request_pr_feedback_sweep(&store, &workflow_id).await?;
    assert_eq!(
        outcome,
        PrFeedbackSweepRequestOutcome::Requested {
            workflow_id: workflow_id.clone(),
            task_id: "task-1".to_string(),
        }
    );
    Ok(())
}
