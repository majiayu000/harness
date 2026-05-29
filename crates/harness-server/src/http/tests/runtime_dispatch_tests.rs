use super::*;

#[tokio::test]
async fn runtime_command_dispatch_tick_enqueues_runtime_jobs() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-a");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\n  runtime_profile: codex-high\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "replanning",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:123"),
    )
    .with_id("issue-123")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 123,
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("replan_issue", "replan-1");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;

    let tick = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "codex-high",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;

    assert_eq!(tick.enqueued, 1);
    assert_eq!(tick.already_dispatched, 0);
    assert_eq!(tick.skipped, 0);
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].runtime_profile, "codex-high");
    let workflow_cfg = harness_core::config::workflow::load_workflow_config(&project_root)?;
    let inherited_profile = harness_workflow::runtime::RuntimeProfile::new(
        "codex-high",
        harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
    );
    let expected_profile_manifest = super::background::runtime_profile_manifest_definition(
        &project_root,
        &state.core.server.config,
        &workflow_cfg.runtime_dispatch,
        &inherited_profile,
    )?;
    let persisted_profile_manifest = store
        .get_definition(
            &expected_profile_manifest.id,
            expected_profile_manifest.version,
        )
        .await?
        .expect("runtime profile manifest definition should be persisted");
    assert_eq!(
        persisted_profile_manifest.metadata["default_profile"]["name"],
        "codex-high"
    );
    assert_eq!(
        persisted_profile_manifest.source_path,
        Some(
            project_root
                .join("WORKFLOW.md")
                .to_string_lossy()
                .into_owned()
        )
    );
    assert_eq!(
        store.commands_for(&workflow.id).await?[0].status,
        "dispatched"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatch_tick_fails_command_when_workflow_config_is_malformed(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-malformed-runtime");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch: [\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "replanning",
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
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("replan_issue", "replan-124");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;

    let tick = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "server-fallback",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;

    assert_eq!(tick.enqueued, 0);
    assert_eq!(tick.already_dispatched, 0);
    assert_eq!(tick.skipped, 1);
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    assert_eq!(store.commands_for(&workflow.id).await?[0].status, "failed");
    let events = store.events_for(&workflow.id).await?;
    let config_event = events
        .iter()
        .find(|event| event.event_type == "WorkflowRuntimeConfigError")
        .expect("config error event should be recorded");
    assert_eq!(config_event.event["command_id"], command_id);
    assert!(config_event.event["reason"]
        .as_str()
        .expect("reason should be a string")
        .contains("failed to load WORKFLOW.md"));
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatch_tick_retries_non_workflow_config_errors() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-malformed-project-config");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let harness_dir = project_root.join(".harness");
    std::fs::create_dir(&harness_dir)?;
    std::fs::write(harness_dir.join("config.toml"), "agent = [")?;

    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "replanning",
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
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("replan_issue", "replan-125");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;

    let err = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "server-fallback",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await
    .expect_err("non-WORKFLOW config errors should remain retryable");

    assert!(format!("{err:#}").contains("failed to load project config"));
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    assert_eq!(
        store.commands_for(&workflow.id).await?[0].status,
        "dispatching"
    );
    let events = store.events_for(&workflow.id).await?;
    assert!(!events
        .iter()
        .any(|event| event.event_type == "WorkflowRuntimeConfigError"));
    Ok(())
}

#[tokio::test]
async fn runtime_pr_feedback_sweep_tick_enqueues_runtime_command() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-feedback");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\npr_feedback:\n  enabled: true\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:226"),
    )
    .with_id("issue-226")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 226,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-226",
    }));
    store.upsert_instance(&workflow).await?;

    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 10).await?;

    assert_eq!(tick.requested, 1);
    assert_eq!(tick.active_command_exists, 0);
    assert_eq!(tick.skipped, 0);
    assert_eq!(tick.rejected, 0);
    let updated = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "awaiting_feedback");
    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.command_type,
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow
    );
    assert_eq!(
        commands[0].command.command["definition_id"],
        harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID
    );
    assert_eq!(
        commands[0].command.command["child_activity"],
        harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY
    );
    Ok(())
}

#[tokio::test]
async fn runtime_pr_feedback_sweep_recovers_pr_binding_from_bind_pr_command() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-feedback-recovery");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\npr_feedback:\n  enabled: true\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:230"),
    )
    .with_id("issue-230")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 230,
        "task_id": "runtime-task-230",
    }));
    store.upsert_instance(&workflow).await?;
    let bind_pr = harness_workflow::runtime::WorkflowCommand::bind_pr(
        80,
        "https://github.com/owner/repo/pull/80",
        "issue-230-bind-pr-80",
    );
    store
        .enqueue_command_with_status(&workflow.id, None, &bind_pr, "skipped")
        .await?;

    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 10).await?;

    assert_eq!(tick.requested, 1);
    assert_eq!(tick.skipped, 0);
    let updated = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "awaiting_feedback");
    assert_eq!(updated.data["pr_number"], 80);
    assert_eq!(
        updated.data["pr_url"],
        "https://github.com/owner/repo/pull/80"
    );
    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands.len(), 2);
    assert_eq!(
        commands[1].command.command_type,
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow
    );
    Ok(())
}

#[tokio::test]
async fn runtime_repo_backlog_poll_tick_enqueues_runtime_command() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-backlog");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nrepo_backlog:\n  enabled: true\n  batch_limit: 5\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repos: vec![harness_core::config::intake::GitHubRepoConfig {
            repo: "owner/repo".to_string(),
            label: "harness".to_string(),
            project_root: Some(project_root.to_string_lossy().into_owned()),
        }],
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        &project_root,
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");

    let tick = super::background::run_runtime_repo_backlog_poll_tick(&state, 10).await?;

    assert_eq!(tick.requested, 1);
    assert_eq!(tick.active_command_exists, 0);
    assert_eq!(tick.skipped, 0);
    assert_eq!(tick.rejected, 0);
    let instances = store
        .list_instances_by_definition(
            harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
            None,
            None,
        )
        .await?;
    assert_eq!(instances.len(), 1);
    let workflow_id = instances[0].id.clone();
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("repo backlog workflow should exist");
    assert_eq!(instance.state, "scanning");
    assert_eq!(instance.data["label"], "harness");
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.activity_name(),
        Some(harness_workflow::runtime::REPO_BACKLOG_POLL_ACTIVITY)
    );
    Ok(())
}

#[tokio::test]
async fn runtime_repo_backlog_poll_tick_skips_malformed_workflow_config() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-backlog-malformed");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nrepo_backlog: [\n---\n",
    )?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repos: vec![harness_core::config::intake::GitHubRepoConfig {
            repo: "owner/repo".to_string(),
            label: "harness".to_string(),
            project_root: Some(project_root.to_string_lossy().into_owned()),
        }],
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        &project_root,
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");

    let tick = super::background::run_runtime_repo_backlog_poll_tick(&state, 10).await?;

    assert_eq!(tick.requested, 0);
    assert_eq!(tick.active_command_exists, 0);
    assert_eq!(tick.skipped, 1);
    assert_eq!(tick.rejected, 0);
    let instances = store
        .list_instances_by_definition(
            harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
            None,
            None,
        )
        .await?;
    assert!(instances.is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_pr_feedback_sweep_limit_ignores_skipped_workflows() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-feedback-limit");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\npr_feedback:\n  enabled: true\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let valid_workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:227"),
    )
    .with_id("issue-227")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 227,
        "pr_number": 78,
        "pr_url": "https://github.com/owner/repo/pull/78",
        "task_id": "runtime-task-227",
    }));
    store.upsert_instance(&valid_workflow).await?;
    let skipped_workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:228"),
    )
    .with_id("issue-228")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 228,
        "task_id": "runtime-task-228",
    }));
    store.upsert_instance(&skipped_workflow).await?;

    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 1).await?;

    assert_eq!(tick.requested, 1);
    assert_eq!(tick.skipped, 1);
    assert_eq!(store.commands_for(&valid_workflow.id).await?.len(), 1);
    assert!(store.commands_for(&skipped_workflow.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_pr_feedback_sweep_respects_project_runtime_policy() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-feedback-disabled-runtime");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\npr_feedback:\n  enabled: true\nruntime_dispatch:\n  enabled: false\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:229"),
    )
    .with_id("issue-229")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 229,
        "pr_number": 79,
        "pr_url": "https://github.com/owner/repo/pull/79",
        "task_id": "runtime-task-229",
    }));
    store.upsert_instance(&workflow).await?;

    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 10).await?;

    assert_eq!(tick.requested, 0);
    assert_eq!(tick.skipped, 1);
    assert!(store.commands_for(&workflow.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatch_tick_uses_command_project_policy_when_server_root_disabled(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let server_root = dir.path().join("server-root");
    let project_root = dir.path().join("project-root");
    std::fs::create_dir(&server_root)?;
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        server_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: false\n  runtime_profile: server-disabled\nruntime_worker:\n  enabled: false\n---\n",
    )?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\n  runtime_profile: project-runtime\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        &server_root,
        harness_core::config::HarnessConfig::default(),
        harness_agents::registry::AgentRegistry::new("test"),
    )
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
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:224"),
    )
    .with_id("issue-224")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 224,
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("implement_issue", "impl-224");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;

    let tick = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "server-disabled",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;

    assert_eq!(tick.enqueued, 1);
    assert_eq!(tick.already_dispatched, 0);
    assert_eq!(tick.skipped, 0);
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].runtime_profile, "project-runtime");
    assert_eq!(
        store.commands_for(&workflow.id).await?[0].status,
        "dispatched"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatch_tick_skips_when_command_project_runtime_disabled(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-root");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\n  runtime_profile: project-runtime\nruntime_worker:\n  enabled: false\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:225"),
    )
    .with_id("issue-225")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 225,
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("implement_issue", "impl-225");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;

    let tick = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "server-fallback",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;

    assert_eq!(tick.enqueued, 0);
    assert_eq!(tick.already_dispatched, 0);
    assert_eq!(tick.skipped, 1);
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    assert_eq!(store.commands_for(&workflow.id).await?[0].status, "skipped");
    Ok(())
}
