use super::*;

#[tokio::test]
async fn runtime_job_worker_replays_auto_submit_without_duplicate_child_side_effects(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-auto-submit-replay");
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
    .with_id("repo-backlog-auto-submit-replay")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "repo-backlog:owner/repo:issue:129:start",
        serde_json::json!({
            "definition_id": "github_issue_pr",
            "subject_key": "issue:129",
            "repo": "owner/repo",
            "labels": ["harness"],
            "source": "github",
            "external_id": "129",
            "auto_submit": true,
        }),
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id.clone(),
                "command_id": command_id.clone(),
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": command.runtime_activity_key(),
                "command": command.command.clone(),
            }),
        )
        .await?;
    let claimed = store
        .claim_next_runtime_job("crashed-worker", Utc::now() - chrono::Duration::seconds(1))
        .await?
        .expect("runtime job should be claimable for the simulated crashed worker");
    assert_eq!(claimed.id, runtime_job.id);

    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 129);
    let child = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "discovered",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:129"),
    )
    .with_id(child_id.clone())
    .with_parent(parent.id.clone())
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 129,
        "started_by_runtime_job_id": runtime_job.id.clone(),
        "started_by_command_id": command_id.clone(),
    }));
    store.upsert_instance(&child).await?;
    store
        .append_event(
            &child_id,
            "ChildWorkflowStarted",
            "workflow_runtime_worker",
            serde_json::json!({
                "parent_workflow_id": parent.id.clone(),
                "runtime_job_id": runtime_job.id.clone(),
                "command_id": command_id.clone(),
                "definition_id": "github_issue_pr",
                "subject_key": "issue:129",
            }),
        )
        .await?;
    let task_id = crate::task_runner::TaskId::from_str("repo-backlog:owner/repo:issue:129");
    let labels = vec!["harness".to_string()];
    crate::workflow_runtime_submission::record_issue_submission(
        store,
        crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
            project_root: std::path::Path::new(&project_id),
            repo: Some("owner/repo"),
            issue_number: 129,
            task_id: &task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: Some("github"),
            external_id: Some("129"),
        },
    )
    .await?;
    let mut submitted_child = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should exist after submission");
    if let Some(data) = submitted_child.data.as_object_mut() {
        data.remove("submission_id");
        data.remove("task_id");
        data.remove("task_ids");
    }
    store.upsert_instance(&submitted_child).await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    assert_eq!(tick.failed, 0);
    let child_after = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should still exist");
    assert_eq!(child_after.state, "planning");
    let child_events = store.events_for(&child_id).await?;
    assert_eq!(
        child_events
            .iter()
            .filter(|event| event.event_type == "ChildWorkflowStarted")
            .count(),
        1
    );
    assert_eq!(
        child_events
            .iter()
            .filter(|event| event.event_type == "IssueSubmitted")
            .count(),
        1
    );
    let child_commands = store.commands_for(&child_id).await?;
    assert_eq!(child_commands.len(), 1);
    assert_eq!(
        child_commands[0].command.activity_name(),
        Some("plan_issue")
    );
    let child_decisions = store.decisions_for(&child_id).await?;
    assert_eq!(child_decisions.len(), 1);
    let parent_after = store
        .get_instance("repo-backlog-auto-submit-replay")
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "idle");
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_completes_auto_submit_after_issue_submitted_event_only(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-auto-submit-event-only");
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
    .with_id("repo-backlog-auto-submit-event-only")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "repo-backlog:owner/repo:issue:130:start",
        serde_json::json!({
            "definition_id": "github_issue_pr",
            "subject_key": "issue:130",
            "repo": "owner/repo",
            "labels": ["harness"],
            "source": "github",
            "external_id": "130",
            "auto_submit": true,
        }),
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id.clone(),
                "command_id": command_id.clone(),
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": command.runtime_activity_key(),
                "command": command.command.clone(),
            }),
        )
        .await?;
    let claimed = store
        .claim_next_runtime_job("crashed-worker", Utc::now() - chrono::Duration::seconds(1))
        .await?
        .expect("runtime job should be claimable for the simulated crashed worker");
    assert_eq!(claimed.id, runtime_job.id);

    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 130);
    let child = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "discovered",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:130"),
    )
    .with_id(child_id.clone())
    .with_parent(parent.id.clone())
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 130,
        "started_by_runtime_job_id": runtime_job.id.clone(),
        "started_by_command_id": command_id.clone(),
    }));
    store.upsert_instance(&child).await?;
    store
        .append_event(
            &child_id,
            "ChildWorkflowStarted",
            "workflow_runtime_worker",
            serde_json::json!({
                "parent_workflow_id": parent.id.clone(),
                "runtime_job_id": runtime_job.id.clone(),
                "command_id": command_id.clone(),
                "definition_id": "github_issue_pr",
                "subject_key": "issue:130",
            }),
        )
        .await?;
    store
        .append_event(
            &child_id,
            "IssueSubmitted",
            "workflow_runtime_submission",
            serde_json::json!({
                "task_id": "repo-backlog:owner/repo:issue:130",
                "repo": "owner/repo",
                "issue_number": 130,
                "labels": ["harness"],
                "force_execute": false,
                "additional_prompt": null,
                "depends_on": [],
                "dependencies_blocked": false,
                "execution_path": "workflow_runtime",
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
    let child_after = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should still exist");
    assert_eq!(child_after.state, "planning");
    assert_eq!(
        child_after
            .data
            .get("task_id")
            .and_then(serde_json::Value::as_str),
        Some("repo-backlog:owner/repo:issue:130")
    );
    let child_events = store.events_for(&child_id).await?;
    assert_eq!(
        child_events
            .iter()
            .filter(|event| event.event_type == "ChildWorkflowStarted")
            .count(),
        1
    );
    assert_eq!(
        child_events
            .iter()
            .filter(|event| event.event_type == "IssueSubmitted")
            .count(),
        1
    );
    let child_decisions = store.decisions_for(&child_id).await?;
    assert_eq!(child_decisions.len(), 1);
    let child_commands = store.commands_for(&child_id).await?;
    assert_eq!(child_commands.len(), 1);
    assert_eq!(
        child_commands[0].command.activity_name(),
        Some("plan_issue")
    );
    let parent_after = store
        .get_instance("repo-backlog-auto-submit-event-only")
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "idle");
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_auto_submit_reopens_after_completed_historical_task_id_data(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-auto-submit-reopen");
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
    .with_id("repo-backlog-auto-submit-reopen")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let first_command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "repo-backlog:owner/repo:issue:131:start:first",
        serde_json::json!({
            "definition_id": "github_issue_pr",
            "subject_key": "issue:131",
            "repo": "owner/repo",
            "labels": ["harness"],
            "source": "github",
            "external_id": "131",
            "auto_submit": true,
        }),
    );
    let first_command_id = store
        .enqueue_command(&parent.id, None, &first_command)
        .await?;
    store
        .enqueue_runtime_job(
            &first_command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id.clone(),
                "command_id": first_command_id,
                "command_type": first_command.command_type,
                "dedupe_key": first_command.dedupe_key.clone(),
                "activity": first_command.runtime_activity_key(),
                "command": first_command.command.clone(),
            }),
        )
        .await?;

    let first_tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(first_tick.succeeded, 1);
    assert_eq!(first_tick.failed, 0);
    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 131);
    let mut child = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should be created by the first submission");
    assert_eq!(child.state, "planning");
    assert_eq!(
        child
            .data
            .get("task_id")
            .and_then(serde_json::Value::as_str),
        Some("repo-backlog:owner/repo:issue:131")
    );
    let first_child_commands = store.commands_for(&child_id).await?;
    assert_eq!(first_child_commands.len(), 1);
    store
        .mark_command_status(
            &first_child_commands[0].id,
            harness_workflow::runtime::WorkflowCommandStatus::Completed,
        )
        .await?;
    child.state = "failed".to_string();
    store.upsert_instance(&child).await?;
    let mut parent = store
        .get_instance("repo-backlog-auto-submit-reopen")
        .await?
        .expect("parent workflow should still exist");
    parent.state = "dispatching".to_string();
    store.upsert_instance(&parent).await?;
    let second_command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "repo-backlog:owner/repo:issue:131:start:second",
        serde_json::json!({
            "definition_id": "github_issue_pr",
            "subject_key": "issue:131",
            "repo": "owner/repo",
            "labels": ["harness"],
            "source": "github",
            "external_id": "131",
            "auto_submit": true,
        }),
    );
    let second_command_id = store
        .enqueue_command(&parent.id, None, &second_command)
        .await?;
    store
        .enqueue_runtime_job(
            &second_command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": second_command_id,
                "command_type": second_command.command_type,
                "dedupe_key": second_command.dedupe_key.clone(),
                "activity": second_command.runtime_activity_key(),
                "command": second_command.command.clone(),
            }),
        )
        .await?;

    let second_tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(second_tick.succeeded, 1);
    assert_eq!(second_tick.failed, 0);
    let reopened_child = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should still exist");
    assert_eq!(reopened_child.state, "planning");
    let child_events = store.events_for(&child_id).await?;
    assert_eq!(
        child_events
            .iter()
            .filter(|event| event.event_type == "IssueSubmitted")
            .count(),
        2
    );
    let child_commands = store.commands_for(&child_id).await?;
    assert_eq!(child_commands.len(), 2);
    let fresh_command = child_commands
        .iter()
        .find(|record| record.id != first_child_commands[0].id)
        .expect("fresh submission command should be enqueued");
    assert_eq!(
        fresh_command.status,
        harness_workflow::runtime::WorkflowCommandStatus::Pending
    );
    assert_eq!(fresh_command.command.activity_name(), Some("plan_issue"));
    assert!(fresh_command.command.dedupe_key.contains(":event:"));
    Ok(())
}
