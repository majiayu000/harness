use super::*;
use harness_core::db::resolve_database_url;

#[tokio::test]
async fn pr_detected_persists_pr_open_state() -> anyhow::Result<()> {
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
    let task_id = TaskId::from_str("task-1");

    record_pr_detected(
        Some(&store),
        PrDetectedRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 123,
            task_id: &task_id,
            pr_number: 77,
            pr_url: "https://github.com/owner/repo/pull/77",
        },
    )
    .await;

    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        123,
    );
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("workflow instance should be persisted");
    assert_eq!(instance.state, "pr_open");
    assert_eq!(
        store.events_for(&workflow_id).await?[0].event_type,
        "PrDetected"
    );
    Ok(())
}

#[tokio::test]
async fn transient_pr_lifecycle_persist_failure_is_retried_and_converges() -> anyhow::Result<()> {
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
    let task_id = TaskId::from_str("transient-pr-lifecycle-persist-failure");
    let _guard = set_pr_lifecycle_persist_test_failures(task_id.as_str(), 2);

    record_pr_detected(
        Some(&store),
        PrDetectedRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 123,
            task_id: &task_id,
            pr_number: 77,
            pr_url: "https://github.com/owner/repo/pull/77",
        },
    )
    .await;

    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        123,
    );
    let instance = store.get_instance(&workflow_id).await?;
    let Some(instance) = instance else {
        anyhow::bail!("workflow instance should be persisted after retry");
    };
    assert_eq!(instance.state, "pr_open");
    let events = store.events_for(&workflow_id).await?;
    assert!(events.iter().any(|event| event.event_type == "PrDetected"));
    assert!(!events
        .iter()
        .any(|event| event.event_type == "PrLifecyclePersistenceFailed"));
    Ok(())
}

#[tokio::test]
async fn persistent_pr_lifecycle_persist_failure_records_operator_event() -> anyhow::Result<()> {
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
    let task_id = TaskId::from_str("persistent-pr-lifecycle-persist-failure");
    let _guard =
        set_pr_lifecycle_persist_test_failures(task_id.as_str(), PR_LIFECYCLE_PERSIST_MAX_ATTEMPTS);

    record_pr_detected(
        Some(&store),
        PrDetectedRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 123,
            task_id: &task_id,
            pr_number: 77,
            pr_url: "https://github.com/owner/repo/pull/77",
        },
    )
    .await;

    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        123,
    );
    let instance = store.get_instance(&workflow_id).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "persistent failure should create a terminal workflow for operator-visible events"
        )
    })?;
    assert_eq!(instance.state, "failed");
    assert!(instance.is_terminal());
    let events = store.events_for(&workflow_id).await?;
    let failure_event = events
        .iter()
        .find(|event| event.event_type == "PrLifecyclePersistenceFailed");
    let Some(failure_event) = failure_event else {
        anyhow::bail!("persistent failure should create an operator-visible workflow event");
    };
    assert_eq!(failure_event.event["operation"], "record_pr_detected");
    assert_eq!(
        failure_event.event["attempts"],
        serde_json::json!(PR_LIFECYCLE_PERSIST_MAX_ATTEMPTS)
    );
    assert_eq!(failure_event.event["issue_number"], 123);
    assert_eq!(failure_event.event["pr_number"], 77);
    assert_eq!(failure_event.event["task_id"], task_id.as_str());
    assert!(failure_event.event["error"]
        .as_str()
        .is_some_and(|error| error.contains("injected PR lifecycle persist failure")));
    Ok(())
}

#[tokio::test]
async fn rejected_new_runtime_decision_persists_initial_instance() -> anyhow::Result<()> {
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
    let instance = issue_instance(
        workflow_id.clone(),
        project_root.to_string_lossy().into_owned(),
        Some("owner/repo".to_string()),
        123,
        "pr_open",
    );
    let decision = WorkflowDecision::new(
        &workflow_id,
        "awaiting_feedback",
        "record_feedback",
        "addressing_feedback",
        "intentionally stale observed state",
    );

    let outcome = commit_runtime_decision(
        &store,
        instance.clone(),
        true,
        decision,
        "FeedbackFound",
        "workflow_runtime_pr_feedback_test",
        json!({ "issue_number": 123, "pr_number": 77 }),
        instance.data.clone(),
    )
    .await?;

    assert!(matches!(
        outcome,
        RuntimeDecisionCommitOutcome::Rejected { .. }
    ));
    let loaded = store
        .get_instance(&workflow_id)
        .await?
        .expect("rejected initial transition should still persist the workflow instance");
    assert_eq!(loaded.state, "pr_open");
    assert_eq!(store.events_for(&workflow_id).await?.len(), 1);
    let decisions = store.decisions_for(&workflow_id).await?;
    assert_eq!(decisions.len(), 1);
    assert!(!decisions[0].accepted);
    Ok(())
}

#[tokio::test]
async fn pr_feedback_ready_to_merge_updates_parent_workflow_after_local_review(
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
    let task_id = TaskId::from_str("task-1");
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        123,
    );
    record_pr_detected(
        Some(&store),
        PrDetectedRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 123,
            task_id: &task_id,
            pr_number: 77,
            pr_url: "https://github.com/owner/repo/pull/77",
        },
    )
    .await;

    record_local_review_passed(
        Some(&store),
        LocalReviewPassedRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: Some(123),
            task_id: &task_id,
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            summary: "Local agent review approved the PR.",
        },
    )
    .await;

    let commands_after_local_review = store.commands_for(&workflow_id).await?;
    assert!(
        commands_after_local_review
            .iter()
            .all(|command| command.command.activity_name()
                != Some(harness_workflow::runtime::LOCAL_REVIEW_ACTIVITY)),
        "legacy local review pass must not leave a duplicate run_local_review activity queued"
    );

    record_pr_feedback(
        Some(&store),
        PrFeedbackRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: Some(123),
            task_id: &task_id,
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            outcome: PrFeedbackOutcome::ReadyToMerge,
            summary: "Reviewer approved and validation passed.",
        },
    )
    .await;

    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("workflow instance should be persisted");
    assert_eq!(instance.state, "quality_gate_pending");
    let events = store.events_for(&workflow_id).await?;
    assert!(events
        .iter()
        .any(|event| event.event_type == "PrReadyToMerge"));
    let commands = store.commands_for(&workflow_id).await?;
    assert!(commands.iter().any(|command| {
        command.command.command_type
            == harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow
            && command.command.command["definition_id"]
                == harness_workflow::runtime::QUALITY_GATE_DEFINITION_ID
    }));
    Ok(())
}

#[tokio::test]
async fn pr_feedback_without_issue_requests_local_review_first() -> anyhow::Result<()> {
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
    let task_id = TaskId::from_str("pr-feedback-task");

    record_pr_feedback(
        Some(&store),
        PrFeedbackRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: None,
            task_id: &task_id,
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            outcome: PrFeedbackOutcome::BlockingFeedback,
            summary: "Review found actionable feedback.",
        },
    )
    .await;

    let workflow_id = pr_workflow_id(&project_root.to_string_lossy(), Some("owner/repo"), 77);
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("PR-scoped workflow should be persisted");
    assert_eq!(instance.subject.subject_type, "pr");
    assert_eq!(instance.subject.subject_key, "pr:77");
    assert_eq!(instance.state, "local_review_gate");
    assert_eq!(instance.data["pr_number"], 77);
    assert!(instance.data.get("issue_number").is_none());
    let events = store.events_for(&workflow_id).await?;
    assert!(events
        .iter()
        .any(|event| event.event_type == "LocalReviewRequested"));
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.activity_name(),
        Some(harness_workflow::runtime::LOCAL_REVIEW_ACTIVITY)
    );
    Ok(())
}

#[tokio::test]
async fn pr_hygiene_repair_requests_address_pr_feedback_with_context() -> anyhow::Result<()> {
    let Ok(database_url) = resolve_database_url(None) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let store =
        match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await {
            Ok(store) => store,
            Err(_) => return Ok(()),
        };
    let project_root = dir.path().join("project-hygiene");
    std::fs::create_dir(&project_root)?;
    let task_id =
        synthesized_pr_feedback_task_id(&project_root.to_string_lossy(), Some("owner/repo"), 81);

    let outcome = request_pr_hygiene_repair(
        &store,
        PrHygieneRepairRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            task_id: &task_id,
            pr_number: 81,
            pr_url: Some("https://github.com/owner/repo/pull/81"),
            title: Some("Dirty PR"),
            merge_state_status: Some("DIRTY"),
            head_oid: Some("abc123"),
            updated_at: Some("2026-06-10T00:00:00Z"),
            observed_at: "2026-06-12T00:00:00Z",
            dirty_age_secs: 172800,
            dirty_age_to_repair_secs: 172800,
            dirty_age_to_comment_secs: 604800,
            rebase_needed_label: "rebase-needed",
        },
    )
    .await?;

    let workflow_id = match outcome {
        PrFeedbackSweepRequestOutcome::Requested { workflow_id, .. } => workflow_id,
        other => anyhow::bail!("expected hygiene repair request, got {other:?}"),
    };
    let Some(instance) = store.get_instance(&workflow_id).await? else {
        anyhow::bail!("workflow should exist");
    };
    assert_eq!(instance.state, "addressing_feedback");
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.activity_name(),
        Some("address_pr_feedback")
    );
    assert_eq!(commands[0].command.command["source"], "pr_hygiene");
    assert_eq!(
        commands[0].command.command["hygiene"]["dirty_age_secs"],
        172800
    );
    assert_eq!(
        commands[0].command.command["hygiene"]["rebase_needed_label"],
        "rebase-needed"
    );
    Ok(())
}

#[tokio::test]
async fn pr_feedback_without_issue_uses_bound_workflow_for_local_review() -> anyhow::Result<()> {
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
    let task_id = TaskId::from_str("pr-feedback-task");
    let issue_workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        123,
    );
    upsert_github_issue_pr_definition(&store).await?;
    let issue_workflow = issue_instance(
        issue_workflow_id.clone(),
        project_root.to_string_lossy().into_owned(),
        Some("owner/repo".to_string()),
        123,
        "pr_open",
    )
    .with_data(json!({
        "project_id": project_root.to_string_lossy(),
        "repo": "owner/repo",
        "issue_number": 123,
        "task_id": "issue-task",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    store.upsert_instance(&issue_workflow).await?;

    record_pr_feedback(
        Some(&store),
        PrFeedbackRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: None,
            task_id: &task_id,
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            outcome: PrFeedbackOutcome::ReadyToMerge,
            summary: "Reviewer approved and validation passed.",
        },
    )
    .await;

    let instance = store
        .get_instance(&issue_workflow_id)
        .await?
        .expect("issue workflow should still exist");
    assert_eq!(instance.state, "local_review_gate");
    assert_eq!(instance.data["issue_number"], 123);
    assert_eq!(instance.data["pr_number"], 77);
    let pr_scoped_id = pr_workflow_id(&project_root.to_string_lossy(), Some("owner/repo"), 77);
    assert!(store.get_instance(&pr_scoped_id).await?.is_none());
    let commands = store.commands_for(&issue_workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.activity_name(),
        Some(harness_workflow::runtime::LOCAL_REVIEW_ACTIVITY)
    );
    Ok(())
}

#[tokio::test]
async fn pr_merged_without_issue_creates_pr_scoped_done_workflow() -> anyhow::Result<()> {
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
    let task_id = TaskId::from_str("pr-feedback-task");

    record_pr_merged(
        Some(&store),
        PrMergedRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: None,
            task_id: &task_id,
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            summary: "PR merged externally.",
        },
    )
    .await;

    let workflow_id = pr_workflow_id(&project_root.to_string_lossy(), Some("owner/repo"), 77);
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("PR-scoped workflow should be persisted");
    assert_eq!(instance.state, "done");
    assert_eq!(instance.data["pr_number"], 77);
    assert!(instance.data.get("issue_number").is_none());
    let events = store.events_for(&workflow_id).await?;
    assert!(events.iter().any(|event| event.event_type == "PrMerged"));
    Ok(())
}

#[tokio::test]
async fn request_local_review_records_runtime_command() -> anyhow::Result<()> {
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
    let instance = issue_instance(
        workflow_id.clone(),
        project_root.to_string_lossy().into_owned(),
        Some("owner/repo".to_string()),
        123,
        "pr_open",
    )
    .with_data(json!({
        "project_id": project_root.to_string_lossy(),
        "repo": "owner/repo",
        "issue_number": 123,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "task-1",
    }));
    store.upsert_instance(&instance).await?;

    let outcome = request_local_review(&store, &workflow_id).await?;
    assert_eq!(
        outcome,
        PrFeedbackSweepRequestOutcome::Requested {
            workflow_id: workflow_id.clone(),
            task_id: "task-1".to_string(),
        }
    );
    let updated = store
        .get_instance(&workflow_id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "local_review_gate");
    assert_eq!(updated.data["last_decision"], "run_local_review");
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.command_type,
        harness_workflow::runtime::WorkflowCommandType::EnqueueActivity
    );
    assert_eq!(
        commands[0].command.activity_name(),
        Some(harness_workflow::runtime::LOCAL_REVIEW_ACTIVITY)
    );
    assert_eq!(commands[0].command.command["pr_number"], 77);

    let claimed = store
        .claim_pending_commands(
            "dispatching-pr-feedback-test",
            chrono::Utc::now() + chrono::Duration::seconds(30),
            1,
        )
        .await?;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].status, "dispatching");

    let second = request_local_review(&store, &workflow_id).await?;
    assert_eq!(
        second,
        PrFeedbackSweepRequestOutcome::NotCandidate {
            workflow_id: workflow_id.clone(),
            state: "local_review_gate".to_string(),
        }
    );
    assert_eq!(store.commands_for(&workflow_id).await?.len(), 1);
    Ok(())
}

mod suppression;
