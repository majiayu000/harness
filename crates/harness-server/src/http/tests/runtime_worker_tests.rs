use super::*;

#[tokio::test]
async fn runtime_job_worker_tick_runs_registered_agent_and_completes_job() -> anyhow::Result<()> {
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
    let agent = RuntimeStreamAgent::new();
    let mut registry = harness_agents::registry::AgentRegistry::new("codex");
    registry.register("codex", agent.clone());
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
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:124"),
    )
    .with_id("issue-124")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 124,
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("implement_issue", "impl-1");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let mut runtime_profile = harness_workflow::runtime::RuntimeProfile::new(
        "codex-default",
        harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
    );
    runtime_profile.model = Some("gpt-runtime".to_string());
    runtime_profile.reasoning_effort = Some("medium".to_string());
    runtime_profile.sandbox = Some("read-only".to_string());
    runtime_profile.approval_policy = Some("on-request".to_string());
    runtime_profile.timeout_secs = Some(300);
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": workflow.id.clone(),
                "command_id": command_id.clone(),
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

    assert_eq!(tick.succeeded, 1);
    assert_eq!(tick.failed, 0);
    assert_eq!(tick.cancelled, 0);
    assert!(!tick.idle);
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    assert_eq!(
        completed.status,
        harness_workflow::runtime::RuntimeJobStatus::Succeeded
    );
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "implement_issue");
    assert_eq!(output.summary, "runtime done");
    let events = store.runtime_events_for(&runtime_job.id).await?;
    assert_eq!(events.len(), 4);
    assert_eq!(events[0].event_type, "RuntimeJobClaimed");
    assert_eq!(events[1].event_type, "RuntimeTurnStarted");
    assert_eq!(events[2].event_type, "RuntimePromptPrepared");
    let prompt_event = &events[2];
    assert_eq!(
        prompt_event.event["prompt_packet"]["schema"],
        "harness.runtime.prompt_packet.v1"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["required_structured_output"]["validation_commands"],
        "Validation commands run and their results."
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["schema"],
        "harness.runtime.activity_result.v1"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["activity"],
        "implement_issue"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["allowed_error_kinds"][1],
        "timeout"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["allowed_error_kinds"][2],
        "fatal"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["transition_contract"]
            ["on_succeeded"]["reducer_next_state"],
        "pr_open_with_pull_request_artifact_or_done_with_closed_issue_signal_else_blocked"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["agent_summary_contract"]
            ["must_include"][2],
        "PR URL, closed issue evidence, or blocker"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["agent_summary_contract"]
            ["artifacts"]["pull_request"]["fields"][1],
        "pr_url"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["agent_summary_contract"]
            ["signals"]["IssueClosed"],
        "Use when the GitHub issue is confirmed closed and no implementation PR is needed. Include state=closed or state=resolved plus issue_number or issue_url."
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["optional_artifacts"]
            ["workflow_decision"]["allowed_confidence"][2],
        "high"
    );
    let decision_contract = &prompt_event.event["prompt_packet"]["activity_result_schema"]
        ["workflow_decision_contract"];
    assert_eq!(decision_contract["workflow_id"], "issue-124");
    assert_eq!(decision_contract["observed_state"], "implementing");
    assert!(decision_contract["allowed_transitions"]
        .as_array()
        .expect("allowed transitions should be an array")
        .iter()
        .any(|transition| transition["next_state"] == "pr_open"));
    let prompt_packet_digest = prompt_event.event["prompt_packet_digest"]
        .as_str()
        .expect("prompt packet digest should be recorded");
    assert_eq!(prompt_packet_digest.len(), 64);
    assert_eq!(events[3].event_type, "ActivityResultReady");
    let prompt_artifact = output
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "runtime_prompt_packet")
        .expect("runtime output should reference the prompt packet");
    assert_eq!(prompt_artifact.artifact["digest"], prompt_packet_digest);
    let prompts = agent.prompts.lock().await;
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("You are executing a Harness workflow runtime job."));
    assert!(prompts[0].contains("Activity: implement_issue"));
    assert!(prompts[0].contains("Prompt packet:"));
    assert!(prompts[0].contains("activity_result_schema"));
    assert!(prompts[0].contains("required_structured_output"));
    assert!(prompts[0].contains("gpt-runtime"));
    drop(prompts);
    let models = agent.models.lock().await;
    assert_eq!(models.as_slice(), &[Some("gpt-runtime".to_string())]);
    drop(models);
    let reasoning_efforts = agent.reasoning_efforts.lock().await;
    assert_eq!(reasoning_efforts.as_slice(), &[Some("medium".to_string())]);
    drop(reasoning_efforts);
    let sandbox_modes = agent.sandbox_modes.lock().await;
    assert_eq!(sandbox_modes.as_slice(), &[Some(SandboxMode::ReadOnly)]);
    drop(sandbox_modes);
    let approval_policies = agent.approval_policies.lock().await;
    assert_eq!(
        approval_policies.as_slice(),
        &[Some("on-request".to_string())]
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_cancels_job_when_workflow_already_terminal() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let agent = RuntimeStreamAgent::new();
    let mut registry = harness_agents::registry::AgentRegistry::new("codex");
    registry.register("codex", agent.clone());
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
        "cancelled",
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
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("implement_issue", "impl-125");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
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
                "activity": "implement_issue",
                "runtime_profile": harness_workflow::runtime::RuntimeProfile::new(
                    "codex-default",
                    harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
                ),
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 0);
    assert_eq!(tick.failed, 0);
    assert_eq!(tick.cancelled, 1);
    assert!(!tick.idle);
    assert!(agent.prompts.lock().await.is_empty());
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    assert_eq!(
        completed.status,
        harness_workflow::runtime::RuntimeJobStatus::Cancelled
    );
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "implement_issue");
    assert_eq!(
        output.summary,
        "Workflow issue-125 was already terminal (cancelled) before runtime execution."
    );
    assert_eq!(
        store.commands_for(&workflow.id).await?[0].status,
        "cancelled"
    );
    let events = store.runtime_events_for(&runtime_job.id).await?;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "RuntimeJobClaimed");
    assert_eq!(events[1].event_type, "ActivityResultReady");
    Ok(())
}

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
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "dispatching",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::start_child_workflow(
        "github_issue_pr",
        "issue:126",
        "repo-backlog:owner/repo:issue:126:start",
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
    assert_eq!(child.parent_workflow_id.as_deref(), Some("repo-backlog"));
    assert_eq!(child.data["issue_number"], 126);
    assert_eq!(child.data["started_by_runtime_job_id"], runtime_job.id);
    let parent_after = store
        .get_instance("repo-backlog")
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
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "dispatching",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-pr-feedback")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "repo-backlog:owner/repo:pr:1120:feedback",
        serde_json::json!({
            "definition_id": "prompt_task",
            "subject_key": "pr:1120:feedback",
            "repo": "owner/repo",
            "pr_number": 1120,
            "source": "github_pr_feedback",
            "external_id": "pr-feedback:owner/repo:1120",
            "task_id": "repo-backlog:owner/repo:pr:1120:feedback",
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
    let task_id = harness_core::types::TaskId::from_str("repo-backlog:owner/repo:pr:1120:feedback");
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
