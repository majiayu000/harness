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
    assert!(state.intake.github_token_dispatch_snapshot().is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatch_tick_honors_prompt_execution_policy() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("prompt-policy-project");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("prompt", "periodic-review:test"),
    )
    .with_id("prompt-execution-policy")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "execution_policy": {
            "task_kind": "review",
            "agent": "claude",
            "turn_timeout_secs": 47,
            "queue_domain": "review",
            "priority": 1,
        }
    }));
    store.upsert_instance(&workflow).await?;
    let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        "implement_prompt",
        "prompt-policy-implement",
    );
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;

    let tick = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "codex-default",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;

    assert_eq!(tick.enqueued, 1);
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].runtime_kind,
        harness_workflow::runtime::RuntimeKind::ClaudeCode
    );
    assert_eq!(jobs[0].runtime_profile, "claude-default");
    assert_eq!(jobs[0].input["runtime_profile"]["timeout_secs"], 47);
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatch_tick_defers_unavailable_isolation_without_fallback(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-isolation-unavailable");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.isolation.rules = vec![harness_core::config::isolation::IsolationRule {
        trust: harness_core::config::isolation::IsolationTrustClass::NonCollaborator,
        tier: harness_core::config::isolation::IsolationTier::Container,
    }];
    let mut state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    Arc::get_mut(&mut state)
        .expect("unique state")
        .isolation_availability =
        harness_core::config::isolation::IsolationAvailability::new(vec![
            harness_core::config::isolation::IsolationTierStatus::available(
                harness_core::config::isolation::IsolationTier::Host,
            ),
            harness_core::config::isolation::IsolationTierStatus::unavailable(
                harness_core::config::isolation::IsolationTier::Container,
                "docker missing",
            ),
        ]);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "replanning",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:126"),
    )
    .with_id("issue-126")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 126,
        "author_trust_class": "non_collaborator",
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("replan_issue", "replan-126");
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
    assert_eq!(tick.deferred, 1);
    assert_eq!(tick.skipped, 0);
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    let persisted = &store.commands_for(&workflow.id).await?[0];
    assert_eq!(persisted.status, "deferred");
    assert_eq!(persisted.dispatch_attempt_count, 1);
    assert_eq!(persisted.dispatch_claim_generation, 1);
    assert!(persisted.dispatch_not_before.is_some());
    let barrier = persisted
        .dispatch_barrier
        .as_ref()
        .expect("isolation barrier should be persisted");
    assert_eq!(barrier.reason_code.as_str(), "isolation_tier_unavailable");
    assert_eq!(barrier.required_tier.as_deref(), Some("container"));
    assert_eq!(barrier.trust_class.as_deref(), Some("non_collaborator"));
    let events = store.events_for(&workflow.id).await?;
    let event = events
        .iter()
        .find(|event| event.event_type == "WorkflowRuntimeDispatchDeferred")
        .expect("atomic dispatch deferral event should be recorded");
    assert_eq!(event.event["dispatch_barrier"]["command_id"], command_id);
    assert_eq!(
        event.event["dispatch_barrier"]["required_tier"],
        "container"
    );
    assert!(event.event["dispatch_barrier"]["reason"]
        .as_str()
        .expect("reason should be a string")
        .contains("docker missing"));

    sqlx::query(
        "UPDATE workflow_commands
         SET dispatch_not_before = CURRENT_TIMESTAMP - INTERVAL '1 second',
             dispatch_barrier = jsonb_set(
                 dispatch_barrier, '{next_dispatch_at}',
                 to_jsonb(to_char(
                     CURRENT_TIMESTAMP - INTERVAL '1 second',
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'
                 ))
             )
         WHERE id = $1",
    )
    .bind(&command_id)
    .execute(store.pool())
    .await?;
    let still_blocked = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "server-fallback",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;
    assert_eq!(still_blocked.enqueued, 0);
    assert_eq!(still_blocked.deferred, 1);
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    assert!(state.intake.github_token_dispatch_snapshot().is_empty());
    let retried = store
        .get_command(&command_id)
        .await?
        .expect("command exists");
    assert_eq!(retried.status, "deferred");
    assert_eq!(retried.dispatch_attempt_count, 2);
    assert_eq!(retried.dispatch_claim_generation, 2);
    let retry_barrier = retried
        .dispatch_barrier
        .as_ref()
        .expect("retry isolation barrier should be persisted");
    assert_eq!(retry_barrier.required_tier.as_deref(), Some("container"));
    assert_eq!(
        retry_barrier.trust_class.as_deref(),
        Some("non_collaborator")
    );
    assert_eq!(
        store
            .events_for(&workflow.id)
            .await?
            .iter()
            .filter(|event| event.event_type == "WorkflowRuntimeDispatchDeferred")
            .count(),
        2
    );
    sqlx::query(
        "UPDATE workflow_commands
         SET dispatch_not_before = CURRENT_TIMESTAMP - INTERVAL '1 second',
             dispatch_barrier = jsonb_set(
                 dispatch_barrier, '{next_dispatch_at}',
                 to_jsonb(to_char(
                     CURRENT_TIMESTAMP - INTERVAL '1 second',
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'
                 ))
             )
         WHERE id = $1",
    )
    .bind(&command_id)
    .execute(store.pool())
    .await?;
    Arc::get_mut(&mut state)
        .expect("state remains unique during the test")
        .isolation_availability =
        harness_core::config::isolation::IsolationAvailability::new(vec![
            harness_core::config::isolation::IsolationTierStatus::available(
                harness_core::config::isolation::IsolationTier::Host,
            ),
            harness_core::config::isolation::IsolationTierStatus::available(
                harness_core::config::isolation::IsolationTier::Container,
            ),
        ]);
    let repaired = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "server-fallback",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;
    assert_eq!(repaired.enqueued, 1);
    assert_eq!(repaired.deferred, 0);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should remain configured");
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].command_id, command_id);
    assert_eq!(jobs[0].input["isolation"]["tier"], "container");
    let repaired_command = store
        .get_command(&command_id)
        .await?
        .expect("command exists");
    assert_eq!(repaired_command.status, "dispatched");
    assert_eq!(repaired_command.dispatch_attempt_count, 2);
    assert_eq!(repaired_command.dispatch_claim_generation, 3);
    assert!(repaired_command.dispatch_barrier.is_none());
    assert!(repaired_command.dispatch_not_before.is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatch_tick_defers_malformed_workflow_config() -> anyhow::Result<()> {
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
    assert_eq!(tick.deferred, 1);
    assert_eq!(tick.skipped, 0);
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    let persisted = &store.commands_for(&workflow.id).await?[0];
    assert_eq!(persisted.status, "deferred");
    assert_eq!(persisted.dispatch_attempt_count, 1);
    assert_eq!(persisted.dispatch_claim_generation, 1);
    assert!(persisted.dispatch_not_before.is_some());
    assert_eq!(
        persisted
            .dispatch_barrier
            .as_ref()
            .expect("config barrier should be persisted")
            .reason_code
            .as_str(),
        "workflow_config_invalid"
    );
    let events = store.events_for(&workflow.id).await?;
    let config_event = events
        .iter()
        .find(|event| event.event_type == "WorkflowRuntimeDispatchDeferred")
        .expect("atomic config deferral event should be recorded");
    assert_eq!(
        config_event.event["dispatch_barrier"]["command_id"],
        command_id
    );
    assert!(config_event.event["dispatch_barrier"]["reason"]
        .as_str()
        .expect("reason should be a string")
        .contains("failed to load WORKFLOW.md"));

    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    sqlx::query(
        "UPDATE workflow_commands
         SET dispatch_not_before = CURRENT_TIMESTAMP - INTERVAL '1 second',
             dispatch_barrier = jsonb_set(
                 dispatch_barrier, '{next_dispatch_at}',
                 to_jsonb(to_char(
                     CURRENT_TIMESTAMP - INTERVAL '1 second',
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'
                 ))
             )
         WHERE id = $1",
    )
    .bind(&command_id)
    .execute(store.pool())
    .await?;
    let repaired = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "server-fallback",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;
    assert_eq!(repaired.enqueued, 1);
    assert_eq!(repaired.deferred, 0);
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].command_id, command_id);
    let repaired_command = store
        .get_command(&command_id)
        .await?
        .expect("command exists");
    assert_eq!(repaired_command.status, "dispatched");
    assert_eq!(repaired_command.dispatch_attempt_count, 1);
    assert_eq!(repaired_command.dispatch_claim_generation, 2);
    assert!(repaired_command.dispatch_barrier.is_none());
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
    assert_eq!(updated.state, "local_review_gate");
    let commands = store.commands_for(&workflow.id).await?;
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
        .enqueue_command_with_status(
            &workflow.id,
            None,
            &bind_pr,
            harness_workflow::runtime::WorkflowCommandStatus::Skipped,
        )
        .await?;

    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 10).await?;

    assert_eq!(tick.requested, 1);
    assert_eq!(tick.skipped, 0);
    let updated = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "local_review_gate");
    assert_eq!(updated.data["pr_number"], 80);
    assert_eq!(
        updated.data["pr_url"],
        "https://github.com/owner/repo/pull/80"
    );
    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands.len(), 2);
    assert_eq!(
        commands[1].command.command_type,
        harness_workflow::runtime::WorkflowCommandType::EnqueueActivity
    );
    assert_eq!(
        commands[1].command.activity_name(),
        Some(harness_workflow::runtime::LOCAL_REVIEW_ACTIVITY)
    );
    assert_eq!(commands[1].command.command["pr_number"], 80);
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
async fn runtime_command_dispatch_tick_defers_disabled_policy_without_agent_metrics(
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
    assert_eq!(tick.deferred, 1);
    assert_eq!(tick.skipped, 0);
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    let persisted = &store.commands_for(&workflow.id).await?[0];
    assert_eq!(persisted.status, "deferred");
    assert_eq!(persisted.dispatch_attempt_count, 1);
    assert_eq!(persisted.dispatch_claim_generation, 1);
    assert_eq!(
        persisted
            .dispatch_barrier
            .as_ref()
            .expect("policy barrier should be persisted")
            .reason_code
            .as_str(),
        "runtime_policy_disabled"
    );
    let metrics_while_disabled = state.intake.github_token_dispatch_snapshot();
    assert!(metrics_while_disabled.is_empty());

    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\n  runtime_profile: project-runtime\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    sqlx::query(
        "UPDATE workflow_commands
         SET dispatch_not_before = CURRENT_TIMESTAMP - INTERVAL '1 second',
             dispatch_barrier = jsonb_set(
                 dispatch_barrier, '{next_dispatch_at}',
                 to_jsonb(to_char(
                     CURRENT_TIMESTAMP - INTERVAL '1 second',
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'
                 ))
             )
         WHERE id = $1",
    )
    .bind(&command_id)
    .execute(store.pool())
    .await?;
    let repaired = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "server-fallback",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;
    assert_eq!(repaired.enqueued, 1);
    assert_eq!(repaired.deferred, 0);
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].command_id, command_id);
    assert_eq!(jobs[0].runtime_profile, "project-runtime");
    let repaired_command = store
        .get_command(&command_id)
        .await?
        .expect("command exists");
    assert_eq!(repaired_command.status, "dispatched");
    assert_eq!(repaired_command.dispatch_attempt_count, 1);
    assert_eq!(repaired_command.dispatch_claim_generation, 2);
    assert!(repaired_command.dispatch_barrier.is_none());
    let metrics_after_repair = state.intake.github_token_dispatch_snapshot();
    assert_eq!(metrics_after_repair.len(), 1);
    assert_eq!(metrics_after_repair[0].agent_implement_issue_count, 1);
    Ok(())
}

fn ready_auto_merge_snapshot(
    head_oid: &str,
) -> crate::github_pr_snapshot::GitHubPrSnapshotArtifacts {
    let normalized_snapshot = serde_json::json!({
        "schema": "harness.github.pr_snapshot.v1",
        "snapshot_source": "server_github_graphql",
        "repo": "owner/repo",
        "pr_number": 77,
        "state": "OPEN",
        "pr_url": "https://github.com/owner/repo/pull/77",
        "url": "https://github.com/owner/repo/pull/77",
        "base_ref": "main",
        "expected_base_ref": "main",
        "head_oid": head_oid,
        "is_draft": false,
        "status_check_rollup_state": "SUCCESS",
        "merge_state_status": "CLEAN",
        "review_decision": "APPROVED",
        "active_unresolved_review_threads_count": 0,
        "review_threads_complete": true,
    });
    crate::github_pr_snapshot::GitHubPrSnapshotArtifacts {
        raw_pr: normalized_snapshot.clone(),
        normalized_snapshot,
    }
}

fn auto_merge_policy(
    require_review_threads_resolved: bool,
    require_clean_merge_state: bool,
) -> harness_core::config::intake::ResolvedGitHubAutoMergePolicy {
    harness_core::config::intake::ResolvedGitHubAutoMergePolicy {
        enabled: true,
        method: harness_core::config::intake::GitHubMergeMethod::Squash,
        delete_branch: false,
        require_review_threads_resolved,
        require_clean_merge_state,
        merge_execution: harness_core::config::intake::GitHubMergeExecution::Agent,
        verify_merge_completion: true,
    }
}

#[test]
fn auto_merge_snapshot_gate_accepts_ready_matching_head() -> anyhow::Result<()> {
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:77"),
    )
    .with_id("issue-77")
    .with_data(serde_json::json!({
        "repo": "owner/repo",
        "issue_number": 77,
        "pr_number": 77,
        "pr_head_sha": "abc123",
    }));

    let outcome = super::auto_merge::prepare_auto_merge_workflow_from_snapshot(
        &workflow,
        &ready_auto_merge_snapshot("abc123"),
        &auto_merge_policy(true, true),
    )?;

    let super::auto_merge::AutoMergeSnapshotGate::Ready(prepared) = outcome else {
        panic!("matching ready snapshot should pass auto-merge gate");
    };
    assert_eq!(prepared.data["merge_policy"], "auto");
    assert_eq!(prepared.data["merge_method"], "squash");
    assert_eq!(prepared.data["merge_delete_branch"], false);
    assert_eq!(prepared.data["merge_require_review_threads_resolved"], true);
    assert_eq!(prepared.data["merge_require_clean_merge_state"], true);
    assert_eq!(prepared.data["merge_execution"], "agent");
    assert_eq!(prepared.data["verify_merge_completion"], true);
    assert_eq!(prepared.data["pr_head_sha"], "abc123");
    assert_eq!(prepared.data["merge_attempted_head_sha"], "abc123");
    assert!(prepared
        .data
        .get("last_remote_fact_hash")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value.starts_with("sha256:")));
    Ok(())
}

#[test]
fn auto_merge_snapshot_gate_accepts_fresh_ready_head_when_stored_head_changed() -> anyhow::Result<()>
{
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:78"),
    )
    .with_id("issue-78")
    .with_data(serde_json::json!({
        "repo": "owner/repo",
        "issue_number": 78,
        "pr_number": 78,
        "pr_head_sha": "old-head",
    }));

    let outcome = super::auto_merge::prepare_auto_merge_workflow_from_snapshot(
        &workflow,
        &ready_auto_merge_snapshot("new-head"),
        &auto_merge_policy(true, true),
    )?;

    let super::auto_merge::AutoMergeSnapshotGate::Ready(prepared) = outcome else {
        panic!("fresh ready snapshot should replace stale stored merge head");
    };
    assert_eq!(prepared.data["pr_head_sha"], "new-head");
    assert_eq!(prepared.data["merge_attempted_head_sha"], "new-head");
    Ok(())
}

#[test]
fn auto_merge_snapshot_gate_persists_fresh_head_when_workflow_head_missing() -> anyhow::Result<()> {
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:80"),
    )
    .with_id("issue-80")
    .with_data(serde_json::json!({
        "repo": "owner/repo",
        "issue_number": 80,
        "pr_number": 80,
    }));

    let outcome = super::auto_merge::prepare_auto_merge_workflow_from_snapshot(
        &workflow,
        &ready_auto_merge_snapshot("fresh-head"),
        &auto_merge_policy(true, true),
    )?;

    let super::auto_merge::AutoMergeSnapshotGate::Ready(prepared) = outcome else {
        panic!("fresh ready snapshot should seed the expected merge head");
    };
    assert_eq!(prepared.data["pr_head_sha"], "fresh-head");
    assert_eq!(prepared.data["merge_attempted_head_sha"], "fresh-head");
    Ok(())
}

#[test]
fn auto_merge_snapshot_gate_honors_relaxed_policy_fields() -> anyhow::Result<()> {
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:79"),
    )
    .with_id("issue-79")
    .with_data(serde_json::json!({
        "repo": "owner/repo",
        "issue_number": 79,
        "pr_number": 79,
        "pr_head_sha": "abc123",
    }));
    let mut snapshot = ready_auto_merge_snapshot("abc123");
    snapshot.normalized_snapshot["merge_state_status"] = serde_json::json!("DIRTY");
    snapshot.normalized_snapshot["active_unresolved_review_threads_count"] = serde_json::json!(2);
    snapshot.normalized_snapshot["review_threads_complete"] = serde_json::json!(false);
    snapshot.raw_pr = snapshot.normalized_snapshot.clone();

    let strict = super::auto_merge::prepare_auto_merge_workflow_from_snapshot(
        &workflow,
        &snapshot,
        &auto_merge_policy(true, true),
    )?;
    assert!(matches!(
        strict,
        super::auto_merge::AutoMergeSnapshotGate::NotReady
    ));

    let relaxed = super::auto_merge::prepare_auto_merge_workflow_from_snapshot(
        &workflow,
        &snapshot,
        &auto_merge_policy(false, false),
    )?;
    let super::auto_merge::AutoMergeSnapshotGate::Ready(prepared) = relaxed else {
        panic!("relaxed policy should pass matching approved snapshot");
    };
    assert_eq!(
        prepared.data["merge_require_review_threads_resolved"],
        false
    );
    assert_eq!(prepared.data["merge_require_clean_merge_state"], false);
    Ok(())
}

#[test]
fn auto_merge_snapshot_gate_rejects_wrong_base_ref() -> anyhow::Result<()> {
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:81"),
    )
    .with_id("issue-81")
    .with_data(serde_json::json!({
        "repo": "owner/repo",
        "issue_number": 81,
        "pr_number": 81,
        "pr_head_sha": "abc123",
        "expected_base_ref": "main",
    }));
    let mut snapshot = ready_auto_merge_snapshot("abc123");
    snapshot.normalized_snapshot["base_ref"] = serde_json::json!("release");
    snapshot.raw_pr = snapshot.normalized_snapshot.clone();

    let outcome = super::auto_merge::prepare_auto_merge_workflow_from_snapshot(
        &workflow,
        &snapshot,
        &auto_merge_policy(true, true),
    )?;

    assert!(matches!(
        outcome,
        super::auto_merge::AutoMergeSnapshotGate::NotReady
    ));
    Ok(())
}

#[test]
fn auto_merge_snapshot_gate_allows_unknown_expected_base() -> anyhow::Result<()> {
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:82"),
    )
    .with_id("issue-82")
    .with_data(serde_json::json!({
        "repo": "owner/repo",
        "issue_number": 82,
        "pr_number": 82,
        "pr_head_sha": "abc123",
    }));
    let mut snapshot = ready_auto_merge_snapshot("abc123");
    snapshot.normalized_snapshot["base_ref"] = serde_json::json!("release");
    snapshot.normalized_snapshot["expected_base_ref"] = serde_json::Value::Null;
    snapshot.raw_pr = snapshot.normalized_snapshot.clone();

    let outcome = super::auto_merge::prepare_auto_merge_workflow_from_snapshot(
        &workflow,
        &snapshot,
        &auto_merge_policy(true, true),
    )?;

    let super::auto_merge::AutoMergeSnapshotGate::Ready(prepared) = outcome else {
        panic!("unknown expected base should not block an otherwise ready snapshot");
    };
    assert!(prepared.data.get("expected_base_ref").is_none());
    Ok(())
}
