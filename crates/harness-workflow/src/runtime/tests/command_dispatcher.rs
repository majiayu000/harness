#[tokio::test]
async fn runtime_command_dispatcher_enqueues_runtime_job_for_activity_command() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "replanning");
    store.upsert_instance(&instance).await?;
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "replanning",
        "run_replan",
        "replanning",
        "Replan before continuing.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-1",
    ));
    let record = WorkflowDecisionRecord::accepted(decision.clone(), None);
    store.record_decision(&record).await?;
    let command_id = store
        .enqueue_command(&instance.id, Some(&record.id), &decision.commands[0])
        .await?;
    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
    );

    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");
    let runtime_job = match outcome {
        CommandDispatchOutcome::Enqueued {
            command_id: dispatched_command_id,
            runtime_job,
        } => {
            assert_eq!(dispatched_command_id, command_id);
            runtime_job
        }
        other => panic!("unexpected dispatch outcome: {other:?}"),
    };
    assert_eq!(runtime_job.command_id, command_id);
    assert_eq!(runtime_job.runtime_kind, RuntimeKind::CodexJsonrpc);
    assert_eq!(runtime_job.runtime_profile, "codex-high");
    assert_eq!(runtime_job.status, RuntimeJobStatus::Pending);
    assert_eq!(
        runtime_job
            .input
            .get("activity")
            .and_then(|activity| activity.as_str()),
        Some("replan_issue")
    );
    assert_eq!(
        runtime_job
            .input
            .get("command")
            .and_then(|command| command.get("activity"))
            .and_then(|activity| activity.as_str()),
        Some("replan_issue")
    );
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        WorkflowCommandStatus::Dispatched
    );
    assert!(dispatcher.dispatch_once().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatcher_prefers_workflow_activity_profile() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let issue = project_issue_instance("/project-a", 123, "replanning");
    let prompt = prompt_task_instance("implementing").with_id("prompt-task-profile");
    store.upsert_instance(&issue).await?;
    store.upsert_instance(&prompt).await?;

    let issue_replan_command =
        WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-profile");
    let issue_implement_command =
        WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement-profile");
    let prompt_replan_command =
        WorkflowCommand::enqueue_activity("replan_issue", "prompt-task-replan-profile");
    let prompt_scan_command =
        WorkflowCommand::enqueue_activity("scan_repo", "prompt-task-scan-profile");
    let issue_replan_command_id = store
        .enqueue_command(&issue.id, None, &issue_replan_command)
        .await?;
    let issue_implement_command_id = store
        .enqueue_command(&issue.id, None, &issue_implement_command)
        .await?;
    let prompt_replan_command_id = store
        .enqueue_command(&prompt.id, None, &prompt_replan_command)
        .await?;
    let prompt_scan_command_id = store
        .enqueue_command(&prompt.id, None, &prompt_scan_command)
        .await?;

    let mut default_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
    default_profile.model = Some("gpt-default".to_string());
    let mut issue_profile = RuntimeProfile::new("codex-issue-high", RuntimeKind::CodexExec);
    issue_profile.model = Some("gpt-issue".to_string());
    issue_profile.timeout_secs = Some(1200);
    let mut replan_profile = RuntimeProfile::new("codex-replan", RuntimeKind::CodexJsonrpc);
    replan_profile.model = Some("gpt-replan".to_string());
    replan_profile.timeout_secs = Some(300);
    let mut issue_replan_profile =
        RuntimeProfile::new("codex-issue-replan", RuntimeKind::CodexExec);
    issue_replan_profile.model = Some("gpt-issue-replan".to_string());
    issue_replan_profile.timeout_secs = Some(90);
    let profile_selector = RuntimeProfileSelector::new(default_profile)
        .with_workflow_profile("github_issue_pr", issue_profile)
        .with_activity_profile("replan_issue", replan_profile)
        .with_workflow_activity_profile("github_issue_pr", "replan_issue", issue_replan_profile);
    let dispatcher = RuntimeCommandDispatcher::with_profile_selector(&store, profile_selector);

    let outcomes = dispatcher.dispatch_pending().await?;
    assert_eq!(outcomes.len(), 4);

    let replan_jobs = store
        .runtime_jobs_for_command(&issue_replan_command_id)
        .await?;
    assert_eq!(replan_jobs.len(), 1);
    assert_eq!(replan_jobs[0].runtime_kind, RuntimeKind::CodexExec);
    assert_eq!(replan_jobs[0].runtime_profile, "codex-issue-replan");
    assert_eq!(
        replan_jobs[0].input["runtime_profile"]["name"],
        "codex-issue-replan"
    );
    assert_eq!(
        replan_jobs[0].input["runtime_profile"]["model"],
        "gpt-issue-replan"
    );
    assert_eq!(replan_jobs[0].input["runtime_profile"]["timeout_secs"], 90);

    let implement_jobs = store
        .runtime_jobs_for_command(&issue_implement_command_id)
        .await?;
    assert_eq!(implement_jobs.len(), 1);
    assert_eq!(implement_jobs[0].runtime_kind, RuntimeKind::CodexExec);
    assert_eq!(implement_jobs[0].runtime_profile, "codex-issue-high");
    assert_eq!(
        implement_jobs[0].input["runtime_profile"]["name"],
        "codex-issue-high"
    );
    assert_eq!(
        implement_jobs[0].input["runtime_profile"]["model"],
        "gpt-issue"
    );
    assert_eq!(
        implement_jobs[0].input["runtime_profile"]["timeout_secs"],
        1200
    );

    let prompt_replan_jobs = store
        .runtime_jobs_for_command(&prompt_replan_command_id)
        .await?;
    assert_eq!(prompt_replan_jobs.len(), 1);
    assert_eq!(
        prompt_replan_jobs[0].runtime_kind,
        RuntimeKind::CodexJsonrpc
    );
    assert_eq!(prompt_replan_jobs[0].runtime_profile, "codex-replan");
    assert_eq!(
        prompt_replan_jobs[0].input["runtime_profile"]["model"],
        "gpt-replan"
    );
    assert_eq!(
        prompt_replan_jobs[0].input["runtime_profile"]["timeout_secs"],
        300
    );

    let prompt_scan_jobs = store
        .runtime_jobs_for_command(&prompt_scan_command_id)
        .await?;
    assert_eq!(prompt_scan_jobs.len(), 1);
    assert_eq!(prompt_scan_jobs[0].runtime_kind, RuntimeKind::CodexJsonrpc);
    assert_eq!(prompt_scan_jobs[0].runtime_profile, "codex-default");
    assert_eq!(
        prompt_scan_jobs[0].input["runtime_profile"]["model"],
        "gpt-default"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatcher_preserves_retry_not_before() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "implementing");
    store.upsert_instance(&instance).await?;
    let not_before = Utc::now() + Duration::seconds(60);
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "issue-123-implement-retry",
        json!({
            "activity": "implement_issue",
            "retry_attempt": 1,
            "retry_not_before": not_before.to_rfc3339(),
        }),
    );
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc),
    );

    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("retry command should dispatch");
    let runtime_job = match outcome {
        CommandDispatchOutcome::Enqueued {
            command_id: dispatched_command_id,
            runtime_job,
        } => {
            assert_eq!(dispatched_command_id, command_id);
            runtime_job
        }
        other => panic!("unexpected dispatch outcome: {other:?}"),
    };

    assert_eq!(runtime_job.not_before, Some(not_before));
    let persisted = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].not_before, Some(not_before));
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatcher_uses_command_type_activity_key_for_child_workflow(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let prompt = prompt_task_instance("implementing").with_id("prompt-task-child");
    store.upsert_instance(&prompt).await?;

    let child_command = WorkflowCommand::start_child_workflow(
        "github_issue_pr",
        "issue:123",
        "prompt-task:issue:123:start",
    );
    let child_command_id = store
        .enqueue_command(&prompt.id, None, &child_command)
        .await?;

    let default_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
    let mut child_profile = RuntimeProfile::new("codex-child", RuntimeKind::CodexExec);
    child_profile.model = Some("gpt-child".to_string());
    let profile_selector = RuntimeProfileSelector::new(default_profile)
        .with_activity_profile("start_child_workflow", child_profile);
    let dispatcher = RuntimeCommandDispatcher::with_profile_selector(&store, profile_selector);

    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("child workflow command should dispatch");
    let runtime_job = match outcome {
        CommandDispatchOutcome::Enqueued {
            command_id,
            runtime_job,
        } => {
            assert_eq!(command_id, child_command_id);
            runtime_job
        }
        other => panic!("unexpected dispatch outcome: {other:?}"),
    };

    assert_eq!(runtime_job.runtime_kind, RuntimeKind::CodexExec);
    assert_eq!(runtime_job.runtime_profile, "codex-child");
    assert_eq!(runtime_job.input["activity"], "start_child_workflow");
    assert_eq!(runtime_job.input["runtime_profile"]["model"], "gpt-child");
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatcher_skips_non_runtime_commands() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "pr_open");
    store.upsert_instance(&instance).await?;
    let command = WorkflowCommand::bind_pr(
        77,
        "https://github.com/owner/repo/pull/77",
        "issue-123-pr-77",
    );
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
    );

    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should be inspected");
    match outcome {
        CommandDispatchOutcome::Skipped {
            command_id: skipped_command_id,
            reason,
        } => {
            assert_eq!(skipped_command_id, command_id);
            assert_eq!(reason, "command does not require runtime execution");
        }
        other => panic!("unexpected dispatch outcome: {other:?}"),
    }
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        WorkflowCommandStatus::Skipped
    );
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatcher_skips_terminal_workflow_before_enqueue() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "cancelled");
    store.upsert_instance(&instance).await?;
    let command =
        WorkflowCommand::enqueue_activity("implement_issue", "issue-123-cancelled-implement");
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc),
    );

    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should be inspected");
    match outcome {
        CommandDispatchOutcome::Skipped {
            command_id: skipped_command_id,
            reason,
        } => {
            assert_eq!(skipped_command_id, command_id);
            assert!(reason.contains("terminal (cancelled)"));
        }
        other => panic!("unexpected dispatch outcome: {other:?}"),
    }
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        WorkflowCommandStatus::Cancelled
    );
    Ok(())
}
