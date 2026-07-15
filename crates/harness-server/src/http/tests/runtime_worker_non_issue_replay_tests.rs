use super::*;

#[tokio::test]
async fn runtime_job_worker_replays_prompt_child_without_duplicate_side_effects(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-prompt-child-replay");
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
    .with_id("prompt-task-prompt-child-replay")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "prompt-task:owner/repo:pr:1120:feedback",
        serde_json::json!({
            "definition_id": harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
            "subject_key": "pr:1120:feedback",
            "repo": "owner/repo",
            "source": "github_pr_feedback",
            "external_id": "pr-feedback:owner/repo:1120",
            "task_id": "prompt-task:owner/repo:pr:1120:feedback",
            "prompt": "Handle unresolved review feedback for PR 1120.",
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

    let task_id = crate::task_runner::TaskId::from_str("prompt-task:owner/repo:pr:1120:feedback");
    let submission = crate::workflow_runtime_submission::record_prompt_submission(
        store,
        crate::workflow_runtime_submission::PromptSubmissionRuntimeContext {
            project_root: std::path::Path::new(&project_id),
            task_id: &task_id,
            prompt: "Handle unresolved review feedback for PR 1120.",
            depends_on: &[],
            serialization_depends_on: &[],
            dependencies_blocked: false,
            source: Some("github_pr_feedback"),
            external_id: Some("pr-feedback:owner/repo:1120"),
            continuation: None,
        },
    )
    .await?;
    let mut child = store
        .get_instance(&submission.workflow_id)
        .await?
        .expect("prompt child workflow should exist after submission");
    child.parent_workflow_id = Some(parent.id.clone());
    store.upsert_instance(&child).await?;
    store
        .append_event(
            &child.id,
            "ChildWorkflowStarted",
            "workflow_runtime_worker",
            serde_json::json!({
                "parent_workflow_id": parent.id.clone(),
                "runtime_job_id": runtime_job.id.clone(),
                "command_id": command_id.clone(),
                "definition_id": harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
                "subject_key": "pr:1120:feedback",
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
    let child_events = store.events_for(&child.id).await?;
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
            .filter(|event| event.event_type == "PromptSubmitted")
            .count(),
        1
    );
    let child_decisions = store.decisions_for(&child.id).await?;
    assert_eq!(child_decisions.len(), 1);
    let child_commands = store.commands_for(&child.id).await?;
    assert_eq!(child_commands.len(), 1);
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_replays_quality_gate_child_without_duplicate_side_effects(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-quality-child-replay");
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
    .with_id("issue-quality-gate-replay-parent")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 227,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "quality-gate:issue-227:77",
        serde_json::json!({
            "definition_id": harness_workflow::runtime::QUALITY_GATE_DEFINITION_ID,
            "subject_key": "pr:77",
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

    let child_id = format!("{}::quality-gate:{}", parent.id, command_id);
    let mut child = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::QUALITY_GATE_DEFINITION_ID,
        1,
        "pending",
        harness_workflow::runtime::WorkflowSubject::new("quality_gate", "pr:77"),
    )
    .with_id(child_id.clone())
    .with_parent(parent.id.clone())
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "parent_workflow_id": parent.id.clone(),
        "started_by_runtime_job_id": runtime_job.id.clone(),
        "started_by_command_id": command_id.clone(),
        "validation_commands": ["cargo check"],
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
                "definition_id": harness_workflow::runtime::QUALITY_GATE_DEFINITION_ID,
                "subject_key": "pr:77",
            }),
        )
        .await?;
    let event = store
        .append_event(
            &child_id,
            "QualityGateRequested",
            "workflow_runtime_worker",
            serde_json::json!({
                "parent_workflow_id": parent.id.clone(),
                "runtime_job_id": runtime_job.id.clone(),
                "command_id": command_id.clone(),
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77",
                "repo": "owner/repo",
            }),
        )
        .await?;
    let validation_commands = vec!["cargo check".to_string()];
    let output = harness_workflow::runtime::build_quality_gate_run_decision(
        &child,
        harness_workflow::runtime::QualityGateDecisionInput {
            reason: "test quality replay",
            validation_commands: &validation_commands,
        },
    );
    let record = harness_workflow::runtime::WorkflowDecisionRecord::accepted(
        output.decision,
        Some(event.id),
    );
    store.record_decision(&record).await?;
    for command in &record.decision.commands {
        store
            .enqueue_command(&child.id, Some(&record.id), command)
            .await?;
    }
    child.state = "pending".to_string();
    store.upsert_instance(&child).await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    let child_after = store
        .get_instance(&child_id)
        .await?
        .expect("quality gate child workflow should still exist");
    assert_eq!(child_after.state, "checking");
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
            .filter(|event| event.event_type == "QualityGateRequested")
            .count(),
        1
    );
    let child_decisions = store.decisions_for(&child_id).await?;
    assert_eq!(child_decisions.len(), 1);
    let child_commands = store.commands_for(&child_id).await?;
    assert_eq!(child_commands.len(), 1);
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_replays_pr_feedback_child_without_duplicate_side_effects(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-pr-feedback-child-replay");
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
    .with_id("issue-pr-feedback-replay-parent")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 226,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "pr-feedback-sweep:issue-226:77",
        serde_json::json!({
            "definition_id": harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
            "subject_key": "pr:77",
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

    let child_id = format!("{}::pr-feedback:{}", parent.id, command_id);
    let mut child = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
        1,
        "pending",
        harness_workflow::runtime::WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id(child_id.clone())
    .with_parent(parent.id.clone())
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 226,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "parent_workflow_id": parent.id.clone(),
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
                "definition_id": harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
                "subject_key": "pr:77",
            }),
        )
        .await?;
    let event = store
        .append_event(
            &child_id,
            "PrFeedbackInspectionRequested",
            "workflow_runtime_worker",
            serde_json::json!({
                "parent_workflow_id": parent.id.clone(),
                "runtime_job_id": runtime_job.id.clone(),
                "command_id": command_id.clone(),
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77",
                "issue_number": 226,
                "repo": "owner/repo",
            }),
        )
        .await?;
    let inspect_dedupe_key = format!("pr-feedback-child:{}:inspect", child_id);
    let output = harness_workflow::runtime::build_pr_feedback_inspect_decision(
        &child,
        harness_workflow::runtime::PrFeedbackInspectDecisionInput {
            dedupe_key: &inspect_dedupe_key,
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            issue_number: Some(226),
            repo: Some("owner/repo"),
            parent_workflow_id: Some(parent.id.as_str()),
            summary: "test pr feedback replay",
        },
    );
    let record = harness_workflow::runtime::WorkflowDecisionRecord::accepted(
        output.decision,
        Some(event.id),
    );
    store.record_decision(&record).await?;
    for command in &record.decision.commands {
        store
            .enqueue_command(&child.id, Some(&record.id), command)
            .await?;
    }
    child.state = "pending".to_string();
    store.upsert_instance(&child).await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    let child_after = store
        .get_instance(&child_id)
        .await?
        .expect("pr feedback child workflow should still exist");
    assert_eq!(child_after.state, "inspecting");
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
            .filter(|event| event.event_type == "PrFeedbackInspectionRequested")
            .count(),
        1
    );
    let child_decisions = store.decisions_for(&child_id).await?;
    assert_eq!(child_decisions.len(), 1);
    let child_commands = store.commands_for(&child_id).await?;
    assert_eq!(child_commands.len(), 1);
    Ok(())
}
