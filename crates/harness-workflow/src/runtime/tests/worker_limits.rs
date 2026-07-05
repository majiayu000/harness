#[tokio::test]
async fn runtime_worker_blocks_when_profile_max_turns_is_exhausted() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = issue_instance("replanning");
    store.upsert_instance(&workflow).await?;
    let mut profile = RuntimeProfile::new("codex-budgeted", RuntimeKind::CodexJsonrpc);
    profile.max_turns = Some(1);

    let first_command = WorkflowCommand::enqueue_activity("replan_issue", "replan-1");
    let first_command_id = store
        .enqueue_command(&workflow.id, None, &first_command)
        .await?;
    store
        .enqueue_runtime_job(
            &first_command_id,
            profile.kind,
            &profile.name,
            json!({
                "workflow_id": workflow.id,
                "command_id": first_command_id,
                "command": first_command.command.clone(),
                "runtime_profile": profile.clone(),
            }),
        )
        .await?;

    let second_command = WorkflowCommand::enqueue_activity("address_pr_feedback", "feedback-1");
    let second_command_id = store
        .enqueue_command(&workflow.id, None, &second_command)
        .await?;
    let mut second_profile = RuntimeProfile::new("codex-budgeted", RuntimeKind::CodexJsonrpc);
    second_profile.max_turns = Some(1);
    store
        .enqueue_runtime_job(
            &second_command_id,
            second_profile.kind,
            &second_profile.name,
            json!({
                "workflow_id": workflow.id,
                "command_id": second_command_id,
                "command": second_command.command.clone(),
                "runtime_profile": second_profile.clone(),
            }),
        )
        .await?;

    let calls = Arc::new(AtomicUsize::new(0));
    let executor = CountingRuntimeExecutor {
        result: ActivityResult::succeeded("replan_issue", "Replan completed."),
        calls: calls.clone(),
    };
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));

    let first_completed = worker
        .run_once(&executor)
        .await?
        .expect("first runtime job should run");
    assert_eq!(first_completed.status, RuntimeJobStatus::Succeeded);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let second_completed = worker
        .run_once(&executor)
        .await?
        .expect("second runtime job should be completed as blocked");
    assert_eq!(second_completed.status, RuntimeJobStatus::Failed);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "executor should not be called after max_turns is exhausted"
    );
    assert_eq!(
        store
            .runtime_turns_started_for_workflow(&workflow.id, None)
            .await?,
        1
    );

    let output: ActivityResult =
        serde_json::from_value(second_completed.output.expect("activity result output"))?;
    assert_eq!(output.status, ActivityStatus::Blocked);
    assert_eq!(output.activity, "address_pr_feedback");
    assert!(output
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("exhausted max_turns"));
    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    assert_eq!(commands[1].status, WorkflowCommandStatus::Blocked);
    Ok(())
}

#[tokio::test]
async fn runtime_worker_records_failed_activity_result() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    enqueue_test_runtime_job(
        &store,
        "command-2",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "implement" }),
    )
    .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1");
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::failed(
            "implement",
            "Codex execution failed.",
            "codex stdin not available",
        ),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should complete failed job");
    assert_eq!(completed.status, RuntimeJobStatus::Failed);
    assert_eq!(
        completed.error.as_deref(),
        Some("codex stdin not available")
    );
    let output: ActivityResult =
        serde_json::from_value(completed.output.expect("activity result output"))?;
    assert_eq!(output.status, ActivityStatus::Failed);
    assert_eq!(output.error.as_deref(), Some("codex stdin not available"));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_cancelled_result_releases_lease() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    enqueue_test_runtime_job(
        &store,
        "command-3",
        RuntimeKind::ClaudeCode,
        "claude-default",
        json!({ "activity": "review" }),
    )
    .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1");
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::cancelled("review", "Runtime job was cancelled."),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should complete cancelled job");
    assert_eq!(completed.status, RuntimeJobStatus::Cancelled);
    assert!(completed.lease.is_none());
    let output: ActivityResult =
        serde_json::from_value(completed.output.expect("activity result output"))?;
    assert_eq!(output.status, ActivityStatus::Cancelled);
    Ok(())
}
