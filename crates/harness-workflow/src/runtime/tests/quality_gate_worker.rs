async fn seed_quality_gate_child_job(
    store: &WorkflowRuntimeStore,
    parent_id: &str,
    child_id: &str,
) -> anyhow::Result<RuntimeJob> {
    let parent = issue_instance("quality_gate_pending").with_id(parent_id);
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        QUALITY_GATE_DEFINITION_ID,
        1,
        "checking",
        WorkflowSubject::new("quality_gate", "pr:77"),
    )
    .with_id(child_id)
    .with_parent(parent.id.clone());
    store.upsert_instance(&child).await?;
    let command = WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "quality-gate-77");
    let command_id = store.enqueue_command(&child.id, None, &command).await?;
    store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": QUALITY_GATE_ACTIVITY }),
        )
        .await
}

#[tokio::test]
async fn runtime_worker_propagates_quality_gate_child_pass_to_parent() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job = seed_quality_gate_child_job(&store, "issue-parent", "quality-gate-child").await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation passed.")
            .with_signal(ActivitySignal::new(
                QUALITY_PASSED_SIGNAL,
                json!({ "validation": "passed" }),
            ))
            .with_validation(ValidationRecord::new("cargo check", "passed")),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance("quality-gate-child")
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "passed");
    let parent_after = store
        .get_instance("issue-parent")
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "ready_to_merge");
    let parent_events = store.events_for("issue-parent").await?;
    assert!(parent_events.iter().any(|event| {
        event.event_type == "RuntimeJobCompleted"
            && event.event["child_workflow_id"] == "quality-gate-child"
    }));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_propagates_quality_gate_child_block_to_parent() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job =
        seed_quality_gate_child_job(&store, "issue-parent-blocked", "quality-gate-child-blocked")
            .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Validation was claimed.")
            .with_signal(ActivitySignal::new(
                QUALITY_PASSED_SIGNAL,
                json!({ "validation": "passed" }),
            )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance("quality-gate-child-blocked")
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "blocked");
    let parent_after = store
        .get_instance("issue-parent-blocked")
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "blocked");
    Ok(())
}

#[tokio::test]
async fn runtime_worker_propagates_quality_gate_child_failure_to_parent() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job =
        seed_quality_gate_child_job(&store, "issue-parent-failed", "quality-gate-child-failed")
            .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::failed(
            QUALITY_GATE_ACTIVITY,
            "Quality gate execution failed.",
            "validation command failed",
        )
        .with_error_kind(ActivityErrorKind::Fatal),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance("quality-gate-child-failed")
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "failed");
    let parent_after = store
        .get_instance("issue-parent-failed")
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "failed");
    Ok(())
}

#[tokio::test]
async fn runtime_worker_does_not_propagate_still_inspecting_pr_feedback_child() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-still-inspecting")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-still-inspecting")
    .with_parent(parent.id.clone());
    store.upsert_instance(&child).await?;
    let command =
        WorkflowCommand::enqueue_activity(PR_FEEDBACK_INSPECT_ACTIVITY, "inspect-pr-feedback-77");
    let command_id = store.enqueue_command(&child.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": PR_FEEDBACK_INSPECT_ACTIVITY }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(
            PR_FEEDBACK_INSPECT_ACTIVITY,
            "PR feedback inspection produced no accepted outcome signal.",
        ),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance(&child.id)
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "inspecting");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "awaiting_feedback");
    let parent_events = store.events_for(&parent.id).await?;
    assert!(
        parent_events
            .iter()
            .all(|event| event.event_type != "RuntimeJobCompleted"),
        "still-inspecting child success must not propagate to parent"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_worker_does_not_propagate_retrying_pr_feedback_child() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-retry")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-retry")
    .with_parent(parent.id.clone())
    .with_data(json!({
        "runtime_retry_policy": {
            "activity_retries": {
                "inspect_pr_feedback": {
                    "max_failed_activity_retries": 2,
                    "retry_delay_secs": 1
                }
            }
        }
    }));
    store.upsert_instance(&child).await?;
    let command =
        WorkflowCommand::enqueue_activity(PR_FEEDBACK_INSPECT_ACTIVITY, "inspect-pr-feedback-77");
    let command_id = store.enqueue_command(&child.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": PR_FEEDBACK_INSPECT_ACTIVITY }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::failed(
            PR_FEEDBACK_INSPECT_ACTIVITY,
            "PR feedback inspection hit a transient API error.",
            "temporary GitHub API error",
        )
        .with_error_kind(ActivityErrorKind::Retryable),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance(&child.id)
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "inspecting");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "awaiting_feedback");
    let parent_events = store.events_for(&parent.id).await?;
    assert!(
        parent_events
            .iter()
            .all(|event| event.event_type != "RuntimeJobCompleted"),
        "retrying child failure must not propagate to parent"
    );
    let child_commands = store.commands_for(&child.id).await?;
    assert!(child_commands.iter().any(|record| {
        record.status == WorkflowCommandStatus::Pending
            && record.command.activity_name() == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
            && record.command.command["retry_attempt"] == 1
    }));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_does_not_propagate_terminal_pr_feedback_child_failure() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-failed-child")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-failed")
    .with_parent(parent.id.clone());
    store.upsert_instance(&child).await?;
    let command =
        WorkflowCommand::enqueue_activity(PR_FEEDBACK_INSPECT_ACTIVITY, "inspect-pr-feedback-77");
    let command_id = store.enqueue_command(&child.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": PR_FEEDBACK_INSPECT_ACTIVITY }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::failed(
            PR_FEEDBACK_INSPECT_ACTIVITY,
            "PR feedback inspection failed permanently.",
            "invalid PR feedback response",
        )
        .with_error_kind(ActivityErrorKind::Fatal),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance(&child.id)
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "failed");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "awaiting_feedback");
    let parent_events = store.events_for(&parent.id).await?;
    assert!(
        parent_events
            .iter()
            .all(|event| event.event_type != "RuntimeJobCompleted"),
        "terminal child failure must not propagate to parent"
    );
    Ok(())
}
