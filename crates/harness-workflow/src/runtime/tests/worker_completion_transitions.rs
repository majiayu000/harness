
#[tokio::test]
async fn runtime_worker_persists_bind_pr_payload_for_pr_open_transition() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 123, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 123,
        "task_id": "task-123",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("implement_issue", "Implementation completed.")
            .with_artifact(ActivityArtifact::new(
                "pull_request",
                json!({
                    "pr_number": 77,
                    "pr_url": "https://github.com/owner/repo/pull/77",
                }),
            )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "pr_open");
    assert_eq!(reloaded.data["project_id"], "/project-a");
    assert_eq!(reloaded.data["issue_number"], 123);
    assert_eq!(reloaded.data["pr_number"], 77);
    assert_eq!(
        reloaded.data["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );

    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    assert_eq!(
        commands[1].command.command_type,
        WorkflowCommandType::BindPr
    );
    assert_ne!(commands[1].status, WorkflowCommandStatus::Pending);
    Ok(())
}

#[tokio::test]
async fn runtime_worker_blocks_invalid_inline_bind_pr_before_persisting_command(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 123, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 123,
        "task_id": "task-123",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let malformed_decision = WorkflowDecision::new(
        &workflow.id,
        "implementing",
        "agent_reported_pr_open",
        "pr_open",
        "The agent reported a PR with incomplete metadata.",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::BindPr,
        "issue-123-bind-pr",
        json!({ "pr_number": 77 }),
    ));
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("implement_issue", "Implementation completed.")
            .with_artifact(ActivityArtifact::new(
                "workflow_decision",
                serde_json::to_value(&malformed_decision).expect("decision should serialize"),
            )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "blocked");
    assert!(reloaded.data.get("pr_number").is_none());
    assert!(reloaded.data.get("pr_url").is_none());

    let commands = store.commands_for(&workflow.id).await?;
    assert!(commands.iter().all(|record| {
        record.command.command_type != WorkflowCommandType::BindPr
            && record.command.dedupe_key != "issue-123-bind-pr"
    }));
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::MarkBlocked
            && record.status == WorkflowCommandStatus::HandledInline
    }));

    let decisions = store.decisions_for(&workflow.id).await?;
    assert!(decisions.iter().any(|record| {
        record.accepted && record.decision.decision == "block_invalid_agent_output"
    }));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_blocks_implementation_success_without_pr_evidence() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 124, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 124,
        "task_id": "task-124",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-124-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("implement_issue", "Implementation completed."),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "blocked");
    assert_eq!(reloaded.data["project_id"], "/project-a");
    assert_eq!(reloaded.data["issue_number"], 124);

    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands[0].id, command_id);
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::MarkBlocked
            && record.status == WorkflowCommandStatus::HandledInline
    }));
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::RequestOperatorAttention
            && record.status == WorkflowCommandStatus::HandledInline
    }));

    let decisions = store.decisions_for(&workflow.id).await?;
    assert!(decisions.iter().any(|record| {
        record.accepted && record.decision.decision == "block_missing_implementation_result"
    }));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_finishes_closed_issue_success_without_pr() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 125, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 125,
        "task_id": "task-125",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-125-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(
            "implement_issue",
            "Issue was already resolved upstream.",
        )
        .with_signal(ActivitySignal::new(
            "IssueAlreadyResolved",
            json!({
                "issue_number": 125,
                "state": "closed",
                "issue_url": "https://github.com/owner/repo/issues/125",
            }),
        )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "done");
    assert_eq!(reloaded.data["project_id"], "/project-a");
    assert_eq!(reloaded.data["issue_number"], 125);
    assert_eq!(
        reloaded.data["closed_issue_evidence"]["issue_url"],
        "https://github.com/owner/repo/issues/125"
    );

    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands[0].id, command_id);
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::MarkDone
            && record.status == WorkflowCommandStatus::HandledInline
    }));

    let decisions = store.decisions_for(&workflow.id).await?;
    assert!(decisions
        .iter()
        .any(|record| record.accepted && record.decision.decision == "finish_closed_issue"));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_propagates_pr_feedback_child_completion_to_parent() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent")
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
    .with_id("pr-feedback-child")
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
    let malformed_parent_decision = WorkflowDecision::new(
        &parent.id,
        "awaiting_feedback",
        "mark_ready_to_merge",
        "ready_to_merge",
        "Child inspection emitted a stale parent decision.",
    )
    .high_confidence();
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(
            PR_FEEDBACK_INSPECT_ACTIVITY,
            "PR feedback child found actionable feedback.",
        )
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(&malformed_parent_decision)?,
        ))
        .with_signal(ActivitySignal::new(
            "FeedbackFound",
            json!({ "pr_number": 77, "count": 1 }),
        )),
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
    assert_eq!(child_after.state, "feedback_found");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "addressing_feedback");
    let parent_commands = store.commands_for(&parent.id).await?;
    assert_eq!(parent_commands.len(), 1);
    assert_eq!(
        parent_commands[0].command.activity_name(),
        Some("address_pr_feedback")
    );
    let parent_events = store.events_for(&parent.id).await?;
    let propagated_event = parent_events
        .iter()
        .find(|event| {
            event.event_type == "RuntimeJobCompleted"
                && event.event["child_workflow_id"] == "pr-feedback-child"
        })
        .expect("child completion should propagate to parent");
    assert!(propagated_event.event["activity_result"]["artifacts"]
        .as_array()
        .expect("activity artifacts should be an array")
        .iter()
        .all(|artifact| artifact["artifact_type"] != "workflow_decision"));
    Ok(())
}

#[tokio::test]
async fn runtime_store_commits_parent_completion_event_decision_and_command() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-transaction")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Runtime child workflow found actionable PR feedback.",
    )
    .with_signal(ActivitySignal::new(
        "FeedbackFound",
        json!({ "pr_number": 77, "count": 1 }),
    ));

    let record = store
        .commit_parent_runtime_completion(
            &parent.id,
            "runtime-1",
            json!({
                "command_id": "child-command-1",
                "runtime_job_id": "child-job-1",
                "child_workflow_id": "pr-feedback-child-transaction",
                "activity_result": result,
            }),
        )
        .await?
        .expect("parent completion should produce a decision");

    assert!(record.accepted);
    assert_eq!(record.decision.decision, "address_pr_feedback");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "addressing_feedback");
    let parent_events = store.events_for(&parent.id).await?;
    assert!(parent_events.iter().any(|event| {
        event.event_type == "RuntimeJobCompleted"
            && event.event["child_workflow_id"] == "pr-feedback-child-transaction"
    }));
    let parent_decisions = store.decisions_for(&parent.id).await?;
    assert_eq!(parent_decisions.len(), 1);
    assert_eq!(parent_decisions[0].id, record.id);
    let parent_commands = store.commands_for(&parent.id).await?;
    assert_eq!(parent_commands.len(), 1);
    assert_eq!(
        parent_commands[0].command.activity_name(),
        Some("address_pr_feedback")
    );
    assert_eq!(
        parent_commands[0].decision_id.as_deref(),
        Some(record.id.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_rolls_back_parent_completion_when_reducer_fails() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-rollback")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;

    let error = store
        .commit_parent_runtime_completion(
            &parent.id,
            "runtime-1",
            json!({
                "command_id": "child-command-rollback",
                "runtime_job_id": "child-job-rollback",
                "child_workflow_id": "pr-feedback-child-rollback",
                "activity_result": "not an activity result",
            }),
        )
        .await
        .expect_err("malformed activity_result should fail the parent completion transaction");

    assert!(
        error.to_string().contains("invalid type"),
        "unexpected error: {error:#}"
    );
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "awaiting_feedback");
    assert!(store.events_for(&parent.id).await?.is_empty());
    assert!(store.decisions_for(&parent.id).await?.is_empty());
    assert!(store.commands_for(&parent.id).await?.is_empty());
    Ok(())
}
