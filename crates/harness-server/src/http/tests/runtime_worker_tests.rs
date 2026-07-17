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
        "pr_open_with_pull_request_artifact_or_done_with_closed_issue_signal_or_blocked_with_scope_too_large_signal_else_blocked"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["agent_summary_contract"]
            ["must_include"][2],
        "PR URL, closed issue evidence, SCOPE_TOO_LARGE decomposition evidence, or blocker"
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
async fn runtime_job_worker_cleans_on_terminal_workspace_after_failed_runtime_attempt(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nbase:\n  require_remote_head: false\nworkspace:\n  strategy: worktree\n  cleanup: on_terminal\n  reuse_existing_workspace: true\n---\n",
    )?;
    init_worktree_git_repo(&project_root)?;
    let workspace_root = dir.path().join("workspaces");
    let mut config = harness_core::config::HarnessConfig::default();
    config.workspace.root = workspace_root.clone();
    config.workspace.root_configured = true;

    let agent = FailingStreamAgent::new("simulated provider outage");
    let mut registry = harness_agents::registry::AgentRegistry::new("codex");
    registry.register("codex", agent.clone());
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        &project_root,
        config.clone(),
        registry,
    )
    .await?;
    let workspace_mgr = Arc::new(crate::workspace::WorkspaceManager::new(
        config.workspace.clone(),
    )?);
    let mut state = match Arc::try_unwrap(state) {
        Ok(state) => state,
        Err(_) => panic!("test state should have one owner before workspace manager injection"),
    };
    state.concurrency.workspace_mgr = Some(workspace_mgr);
    let state = Arc::new(state);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1299"),
    )
    .with_id("issue-1299")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 1299,
        "task_id": "runtime-task-1299",
        "task_ids": ["runtime-task-1299"],
    }));
    store.upsert_instance(&workflow).await?;
    let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        "implement_issue",
        "impl-1299",
    );
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    store
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

    assert_eq!(tick.failed, 1);
    let updated = store
        .get_instance("issue-1299")
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "failed");
    assert!(
        updated
            .data
            .get("failure_reason")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|reason| reason.contains("simulated provider outage")),
        "failed runtime workflow should retain the projected failure reason: {:?}",
        updated.data
    );
    let remaining_workspaces = std::fs::read_dir(&workspace_root)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .count();
    assert_eq!(
        remaining_workspaces, 0,
        "terminal on_terminal cleanup should remove the deterministic runtime workspace"
    );
    assert_eq!(agent.prompts.lock().await.len(), 1);
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

mod child_workflows;
