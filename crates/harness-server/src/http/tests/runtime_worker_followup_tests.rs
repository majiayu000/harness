use super::*;

#[tokio::test]
async fn runtime_job_worker_requeues_pr_feedback_child_inspect_after_stale_dedupe(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-pr-feedback-child-retry");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "awaiting_feedback",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:227"),
    )
    .with_id("issue-pr-feedback-parent-retry")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 227,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-227",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "pr-feedback-sweep:issue-227:77",
        serde_json::json!({
            "definition_id": harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
            "subject_key": "pr:77",
            "child_activity": harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
            "repo": "owner/repo",
            "issue_number": 227,
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
        }),
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": command.runtime_activity_key(),
                "command": command.command.clone(),
            }),
        )
        .await?;
    let child_id = format!("{}::pr-feedback:{}", parent.id, command_id);
    let child = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
        1,
        "pending",
        harness_workflow::runtime::WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id(child_id.clone())
    .with_parent(parent.id.clone());
    store.upsert_instance(&child).await?;
    let stale_dedupe_key = format!("pr-feedback-child:{}:inspect", child_id);
    let stale_command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
        stale_dedupe_key.clone(),
    );
    let stale_command_id = store
        .enqueue_command(&child_id, None, &stale_command)
        .await?;
    store
        .mark_command_status(&stale_command_id, "completed")
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    let child = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should still exist");
    assert_eq!(child.state, "inspecting");
    let child_commands = store.commands_for(&child_id).await?;
    assert_eq!(child_commands.len(), 2);
    let stale = child_commands
        .iter()
        .find(|record| record.id == stale_command_id)
        .expect("stale command should still be recorded");
    assert_eq!(stale.status, "completed");
    let requeued = child_commands
        .iter()
        .find(|record| record.id != stale_command_id)
        .expect("retry command should be recorded");
    assert_eq!(requeued.status, "pending");
    assert_eq!(
        requeued.command.activity_name(),
        Some(harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY)
    );
    assert!(requeued
        .command
        .dedupe_key
        .starts_with(&format!("{}:retry:", stale_dedupe_key)));
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_auto_submits_repo_backlog_child_workflow() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-auto-submit");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "dispatching",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-auto-submit")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "repo-backlog:owner/repo:issue:127:start",
        serde_json::json!({
            "definition_id": "github_issue_pr",
            "subject_key": "issue:127",
            "repo": "owner/repo",
            "labels": ["harness"],
            "source": "github",
            "external_id": "127",
            "auto_submit": true,
        }),
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let activity = command.runtime_activity_key().to_string();
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": activity,
                "command": command.command.clone(),
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 127);
    let child = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should be created");
    assert_eq!(child.state, "planning");
    assert_eq!(child.data["source"], "github");
    assert_eq!(child.data["external_id"], "127");
    let child_commands = store.commands_for(&child_id).await?;
    assert_eq!(child_commands.len(), 1);
    assert_eq!(
        child_commands[0].command.activity_name(),
        Some("plan_issue")
    );
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert!(output
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "child_submission"));
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_auto_submits_repo_backlog_child_with_dependencies() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-auto-submit-deps");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "dispatching",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-auto-submit-deps")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "repo-backlog:owner/repo:issue:128:start",
        serde_json::json!({
            "definition_id": "github_issue_pr",
            "subject_key": "issue:128",
            "repo": "owner/repo",
            "labels": ["harness"],
            "depends_on": [127],
            "source": "github",
            "external_id": "128",
            "auto_submit": true,
        }),
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let activity = command.runtime_activity_key().to_string();
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": activity,
                "command": command.command.clone(),
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 128);
    let child = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should be created");
    assert_eq!(child.state, "awaiting_dependencies");
    assert_eq!(
        child.data["depends_on"][0],
        "repo-backlog:owner/repo:issue:127"
    );
    let child_commands = store.commands_for(&child_id).await?;
    assert!(
        child_commands.is_empty(),
        "dependent issue should not enqueue implement_issue until dependencies release"
    );
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert!(output
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "child_submission"));
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_marks_bound_issue_done_without_agent_turn() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "reconciling",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-mark")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "last_issue_number": 127,
        "last_pr_number": 77,
        "last_pr_url": "https://github.com/owner/repo/pull/77",
    }));
    store.upsert_instance(&parent).await?;
    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 127);
    let child = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:127"),
    )
    .with_id(child_id.clone())
    .with_parent("repo-backlog-mark")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 127,
    }));
    store.upsert_instance(&child).await?;
    let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        "mark_bound_issue_done",
        "repo-backlog:owner/repo:pr:77:merged",
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let activity = command.runtime_activity_key().to_string();
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": activity,
                "command": command.command.clone(),
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    assert_eq!(tick.failed, 0);
    assert_eq!(tick.cancelled, 0);
    let child_after = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should still exist");
    assert_eq!(child_after.state, "done");
    assert_eq!(
        child_after.parent_workflow_id.as_deref(),
        Some("repo-backlog-mark")
    );
    assert_eq!(child_after.data["issue_number"], 127);
    assert_eq!(child_after.data["pr_number"], 77);
    assert_eq!(
        child_after.data["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );
    assert_eq!(
        child_after.data["started_by_runtime_job_id"],
        runtime_job.id
    );
    let parent_after = store
        .get_instance("repo-backlog-mark")
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "idle");
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "mark_bound_issue_done");
    assert_eq!(
        output.status,
        harness_workflow::runtime::ActivityStatus::Succeeded
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_recovers_stale_issue_workflow_without_agent_turn() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "reconciling",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-recover")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "last_issue_number": 128,
        "last_active_task_id": "task-128",
        "last_observed_state": "implementing",
        "last_recovery_reason": "active task disappeared during startup reconciliation",
    }));
    store.upsert_instance(&parent).await?;
    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 128);
    let child = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:128"),
    )
    .with_id(child_id.clone())
    .with_parent("repo-backlog-recover")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 128,
    }));
    store.upsert_instance(&child).await?;
    let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        "recover_issue_workflow",
        "repo-backlog:owner/repo:issue:128:recover",
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let activity = command.runtime_activity_key().to_string();
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": activity,
                "command": command.command.clone(),
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    assert_eq!(tick.failed, 0);
    assert_eq!(tick.cancelled, 0);
    let child_after = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should still exist");
    assert_eq!(child_after.state, "scheduled");
    assert_eq!(
        child_after.parent_workflow_id.as_deref(),
        Some("repo-backlog-recover")
    );
    assert_eq!(child_after.data["issue_number"], 128);
    assert_eq!(child_after.data["previous_state"], "implementing");
    assert_eq!(child_after.data["previous_active_task_id"], "task-128");
    assert_eq!(
        child_after.data["recovery_reason"],
        "active task disappeared during startup reconciliation"
    );
    assert_eq!(
        child_after.data["started_by_runtime_job_id"],
        runtime_job.id
    );
    let parent_after = store
        .get_instance("repo-backlog-recover")
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "idle");
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "recover_issue_workflow");
    assert_eq!(
        output.status,
        harness_workflow::runtime::ActivityStatus::Succeeded
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_applies_runtime_profile_timeout() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nworkspace:\n  strategy: source\n---\n",
    )?;
    let mut registry = harness_agents::registry::AgentRegistry::new("codex");
    registry.register("codex", BlockingAgent::new());
    let state =
        make_test_state_with_workflow_runtime_and_registry(dir.path(), &project_root, registry)
            .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:125"),
    )
    .with_id("issue-125")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 125,
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("implement_issue", "impl-2");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let mut runtime_profile = harness_workflow::runtime::RuntimeProfile::new(
        "codex-default",
        harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
    );
    runtime_profile.timeout_secs = Some(1);
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": workflow.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key,
                "command": command.command,
                "runtime_profile": runtime_profile,
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.failed, 1);
    assert_eq!(tick.succeeded, 0);
    assert_eq!(tick.cancelled, 0);
    assert!(!tick.idle);
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    assert_eq!(
        completed.status,
        harness_workflow::runtime::RuntimeJobStatus::Failed
    );
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "implement_issue");
    assert_eq!(
        output.status,
        harness_workflow::runtime::ActivityStatus::Failed
    );
    assert!(
        output
            .error
            .as_deref()
            .is_some_and(|error| error.contains("timed out")),
        "failed runtime job should include timeout error: {output:?}"
    );
    Ok(())
}
