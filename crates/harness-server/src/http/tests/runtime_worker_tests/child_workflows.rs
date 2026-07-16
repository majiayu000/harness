use super::*;

#[tokio::test]
async fn runtime_job_worker_starts_child_workflow_without_agent_turn() -> anyhow::Result<()> {
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
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("prompt", "owner/repo"),
    )
    .with_id("prompt-task")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::start_child_workflow(
        "github_issue_pr",
        "issue:126",
        "prompt-task:owner/repo:issue:126:start",
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let activity = command.runtime_activity_key().to_string();
    let mut runtime_profile = harness_workflow::runtime::RuntimeProfile::new(
        "codex-default",
        harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
    );
    runtime_profile.max_turns = Some(0);
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

    assert_eq!(tick.succeeded, 1);
    assert_eq!(tick.failed, 0);
    assert_eq!(tick.cancelled, 0);
    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 126);
    let child = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should be created");
    assert_eq!(child.state, "discovered");
    assert_eq!(child.parent_workflow_id.as_deref(), Some("prompt-task"));
    assert_eq!(child.data["issue_number"], 126);
    assert_eq!(child.data["started_by_runtime_job_id"], runtime_job.id);
    let parent_after = store
        .get_instance("prompt-task")
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "implementing");
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "start_child_workflow");
    assert_eq!(
        output.status,
        harness_workflow::runtime::ActivityStatus::Succeeded
    );
    assert_eq!(
        store
            .runtime_turns_started_for_workflow(&parent.id, None)
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_starts_prompt_child_workflow_for_open_pr_feedback() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-pr-feedback-prompt");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("prompt", "owner/repo"),
    )
    .with_id("prompt-task-pr-feedback")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "prompt-task:owner/repo:pr:1120:feedback",
        serde_json::json!({
            "definition_id": "prompt_task",
            "subject_key": "pr:1120:feedback",
            "repo": "owner/repo",
            "pr_number": 1120,
            "source": "github_pr_feedback",
            "external_id": "pr-feedback:owner/repo:1120",
            "task_id": "prompt-task:owner/repo:pr:1120:feedback",
            "prompt": "Handle unresolved review feedback for PR https://github.com/owner/repo/pull/1120.",
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
    assert_eq!(tick.failed, 0);
    let task_id = harness_core::types::TaskId::from_str("prompt-task:owner/repo:pr:1120:feedback");
    let child_id = crate::workflow_runtime_submission::prompt_workflow_id(
        &project_id,
        Some("pr-feedback:owner/repo:1120"),
        &task_id,
    );
    let child = store
        .get_instance(&child_id)
        .await?
        .expect("prompt child workflow should be created");
    assert_eq!(child.definition_id, "prompt_task");
    assert_eq!(
        child.parent_workflow_id.as_deref(),
        Some(parent.id.as_str())
    );
    assert_eq!(child.data["source"], "github_pr_feedback");
    assert_eq!(child.data["external_id"], "pr-feedback:owner/repo:1120");
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "start_child_workflow");
    assert!(output
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "child_submission"));
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_starts_pr_feedback_child_workflow_without_agent_turn(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-pr-feedback-child");
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
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:226"),
    )
    .with_id("issue-pr-feedback-parent")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 226,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-226",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "pr-feedback-sweep:issue-226:77",
        serde_json::json!({
            "definition_id": harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
            "subject_key": "pr:77",
            "child_activity": harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
            "repo": "owner/repo",
            "issue_number": 226,
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
        }),
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
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
                "activity": command.runtime_activity_key(),
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
    assert_eq!(
        store
            .runtime_turns_started_for_workflow(&parent.id, None)
            .await?,
        0
    );
    let children = store
        .list_instances_by_definition(
            harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
            Some(&project_id),
            None,
        )
        .await?;
    assert_eq!(children.len(), 1);
    let child = &children[0];
    assert_eq!(child.state, "inspecting");
    assert_eq!(
        child.parent_workflow_id.as_deref(),
        Some("issue-pr-feedback-parent")
    );
    assert_eq!(child.data["pr_number"], 77);
    assert_eq!(child.data["started_by_runtime_job_id"], runtime_job.id);
    let child_commands = store.commands_for(&child.id).await?;
    assert_eq!(child_commands.len(), 1);
    assert_eq!(
        child_commands[0].command.activity_name(),
        Some(harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY)
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_starts_quality_gate_child_workflow_without_agent_turn(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-quality-gate-child");
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
        "quality_gate_pending",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:227"),
    )
    .with_id("issue-quality-gate-parent")
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
        "quality-gate:issue-227:77",
        serde_json::json!({
            "definition_id": harness_workflow::runtime::QUALITY_GATE_DEFINITION_ID,
            "subject_key": "pr:77",
            "child_activity": harness_workflow::runtime::QUALITY_GATE_ACTIVITY,
            "repo": "owner/repo",
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "validation_commands": ["cargo check"],
        }),
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
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
                "activity": command.runtime_activity_key(),
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
    assert_eq!(
        store
            .runtime_turns_started_for_workflow(&parent.id, None)
            .await?,
        0
    );
    let children = store
        .list_instances_by_definition(
            harness_workflow::runtime::QUALITY_GATE_DEFINITION_ID,
            Some(&project_id),
            None,
        )
        .await?;
    assert_eq!(children.len(), 1);
    let child = &children[0];
    assert_eq!(child.state, "checking");
    assert_eq!(
        child.parent_workflow_id.as_deref(),
        Some("issue-quality-gate-parent")
    );
    assert_eq!(child.data["pr_number"], 77);
    assert_eq!(child.data["started_by_runtime_job_id"], runtime_job.id);
    assert_eq!(child.data["validation_commands"][0], "cargo check");
    let child_commands = store.commands_for(&child.id).await?;
    assert_eq!(child_commands.len(), 1);
    assert_eq!(
        child_commands[0].command.activity_name(),
        Some(harness_workflow::runtime::QUALITY_GATE_ACTIVITY)
    );
    Ok(())
}
