#[tokio::test]
async fn runtime_worker_claims_one_job_once_and_records_events() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job = enqueue_test_runtime_job(
        &store,
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
    )
    .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("check", "Validation passed."),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    assert!(completed.lease.is_none());
    assert!(worker.run_once(&executor).await?.is_none());

    let events = store.runtime_events_for(&completed.id).await?;
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].event_type, "RuntimeJobClaimed");
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[1].event_type, "RuntimeTurnStarted");
    assert_eq!(events[1].sequence, 2);
    assert_eq!(events[2].event_type, "ActivityResultReady");
    assert_eq!(events[2].sequence, 3);
    assert_eq!(
        store
            .runtime_job_count_by_status(RuntimeJobStatus::Pending)
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_get_instance_by_pr_filters_by_project_repo_and_pr() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let matching = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        WorkflowSubject::new("issue", "issue:77"),
    )
    .with_id("project-a::owner/repo::issue:77")
    .with_data(json!({
        "project_id": "project-a",
        "repo": "owner/repo",
        "issue_number": 77,
        "pr_number": 880,
    }));
    let wrong_repo = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        WorkflowSubject::new("issue", "issue:78"),
    )
    .with_id("project-a::owner/other::issue:78")
    .with_data(json!({
        "project_id": "project-a",
        "repo": "owner/other",
        "issue_number": 78,
        "pr_number": 880,
    }));
    let wrong_project = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        WorkflowSubject::new("issue", "issue:79"),
    )
    .with_id("project-b::owner/repo::issue:79")
    .with_data(json!({
        "project_id": "project-b",
        "repo": "owner/repo",
        "issue_number": 79,
        "pr_number": 880,
    }));
    store.upsert_instance(&matching).await?;
    store.upsert_instance(&wrong_repo).await?;
    store.upsert_instance(&wrong_project).await?;

    let found = store
        .get_instance_by_pr("github_issue_pr", "project-a", Some("owner/repo"), 880)
        .await?
        .expect("matching runtime issue workflow should be found");
    assert_eq!(found.id, matching.id);
    assert!(store
        .get_instance_by_pr("github_issue_pr", "project-a", Some("owner/repo"), 881)
        .await?
        .is_none());
    assert!(store
        .get_instance_by_pr("github_issue_pr", "project-b", Some("owner/other"), 880)
        .await?
        .is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_worker_skips_runtime_jobs_before_not_before() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let not_before = Utc::now() + Duration::minutes(5);
    let job = enqueue_test_runtime_job_with_not_before(
        &store,
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
        Some(not_before),
    )
    .await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let executor = CountingRuntimeExecutor {
        result: ActivityResult::succeeded("check", "Validation passed."),
        calls: calls.clone(),
    };
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));

    assert!(worker.run_once(&executor).await?.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .expect("runtime job should still exist");
    assert_eq!(persisted.status, RuntimeJobStatus::Pending);
    assert_eq!(persisted.not_before, Some(not_before));
    assert_eq!(
        store
            .runtime_job_count_by_status(RuntimeJobStatus::Pending)
            .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_reclaims_expired_running_job() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job = enqueue_test_runtime_job(
        &store,
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
    )
    .await?;

    let first_claim = store
        .claim_next_runtime_job("runtime-1", Utc::now() - Duration::minutes(1))
        .await?
        .expect("runtime job should be claimable");
    assert_eq!(first_claim.id, job.id);
    assert_eq!(first_claim.status, RuntimeJobStatus::Running);
    assert_eq!(
        first_claim
            .lease
            .as_ref()
            .expect("lease should exist")
            .owner,
        "runtime-1"
    );
    let first_lease_expires_at = first_claim
        .lease
        .as_ref()
        .expect("lease should exist")
        .expires_at;

    let reclaimed = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .expect("expired running job should be reclaimable");
    assert_eq!(reclaimed.id, job.id);
    assert_eq!(reclaimed.status, RuntimeJobStatus::Running);
    assert_eq!(
        reclaimed.lease.as_ref().expect("lease should exist").owner,
        "runtime-1"
    );
    let reclaimed_lease_expires_at = reclaimed
        .lease
        .as_ref()
        .expect("lease should exist")
        .expires_at;
    assert_ne!(first_lease_expires_at, reclaimed_lease_expires_at);

    let stale_result = ActivityResult::succeeded("check", "Stale worker completed.");
    assert!(
        store
            .complete_runtime_job_if_owned(
                &first_claim.id,
                "runtime-1",
                first_lease_expires_at,
                &stale_result
            )
            .await?
            .is_none(),
        "stale lease completion should be ignored even when owner name is reused"
    );
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .expect("runtime job should still exist");
    assert_eq!(persisted.status, RuntimeJobStatus::Running);
    assert_eq!(
        persisted.lease.as_ref().expect("lease should exist").owner,
        "runtime-1"
    );
    assert!(persisted.output.is_none());

    let current_result = ActivityResult::succeeded("check", "Current worker completed.");
    let completed = store
        .complete_runtime_job_if_owned(
            &reclaimed.id,
            "runtime-1",
            reclaimed_lease_expires_at,
            &current_result,
        )
        .await?
        .expect("current owner completion should be accepted");
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    assert!(completed.lease.is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_store_prioritizes_ready_work_over_other_activity_jobs() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let background_poll = enqueue_test_runtime_job(
        &store,
        "command-background-poll",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "github_issue_poll" }),
    )
    .await?;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let dependency_analysis = enqueue_test_runtime_job(
        &store,
        "command-dependency-analysis",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "analyze_dependencies" }),
    )
    .await?;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let implementation = enqueue_test_runtime_job(
        &store,
        "command-implement",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "implement_issue" }),
    )
    .await?;

    let first = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .ok_or_else(|| anyhow::anyhow!("ready implementation job should be claimed first"))?;
    assert_eq!(first.id, implementation.id);

    let second = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("older non-priority job should be claimed after implementation")
        })?;
    assert_eq!(second.id, background_poll.id);

    let third = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .ok_or_else(|| anyhow::anyhow!("remaining non-priority job should still run"))?;
    assert_eq!(third.id, dependency_analysis.id);
    Ok(())
}

#[tokio::test]
async fn runtime_store_does_not_reclaim_unexpired_running_job() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job = enqueue_test_runtime_job(
        &store,
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
    )
    .await?;

    let first_claim = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .expect("runtime job should be claimable");
    assert_eq!(first_claim.id, job.id);

    assert!(store
        .claim_next_runtime_job("runtime-2", Utc::now() + Duration::minutes(10))
        .await?
        .is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_worker_renews_running_job_lease_until_completion() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store =
        Arc::new(WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?);
    let job = enqueue_test_runtime_job(
        store.as_ref(),
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
    )
    .await?;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let blocking_calls = Arc::new(AtomicUsize::new(0));
    let blocking_executor = BlockingRuntimeExecutor {
        result: ActivityResult::succeeded("check", "Validation passed."),
        calls: blocking_calls.clone(),
        started: Mutex::new(Some(started_tx)),
        finish: Mutex::new(Some(finish_rx)),
    };
    let worker_store = store.clone();
    let worker_handle = tokio::spawn(async move {
        let worker = RuntimeWorker::new(worker_store.as_ref(), "runtime-1")
            .with_lease_ttl(Duration::seconds(2));
        worker.run_once(&blocking_executor).await
    });

    started_rx.await?;
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_executor = CountingRuntimeExecutor {
        result: ActivityResult::succeeded("check", "Second worker should not run."),
        calls: second_calls.clone(),
    };
    let second_worker =
        RuntimeWorker::new(store.as_ref(), "runtime-2").with_lease_ttl(Duration::seconds(2));
    let second_claim = second_worker.run_once(&second_executor).await?;
    let _ = finish_tx.send(());
    let completed = worker_handle
        .await??
        .expect("first worker should complete the runtime job");

    assert!(second_claim.is_none());
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    assert_eq!(blocking_calls.load(Ordering::SeqCst), 1);
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    assert!(completed.lease.is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_worker_records_completion_event_and_command_status() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = issue_instance("replanning");
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("replan_issue", "replan-1");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "replan_issue" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("replan_issue", "Replan completed."),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    assert_eq!(
        store.commands_for(&workflow.id).await?[0].status,
        WorkflowCommandStatus::Completed
    );
    let workflow_events = store.events_for(&workflow.id).await?;
    let event = workflow_events
        .iter()
        .find(|event| event.event_type == "RuntimeJobCompleted")
        .expect("completion event should be appended");
    assert_eq!(event.source, "runtime-1");
    assert_eq!(event.event["command_id"], command_id);
    assert_eq!(
        event.event["command"]["command"]["activity"],
        "replan_issue"
    );
    assert_eq!(event.event["runtime_job_id"], job.id);
    assert_eq!(event.event["runtime_job_status"], "succeeded");
    assert_eq!(
        event.event["activity_result"]["summary"],
        "Replan completed."
    );
    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "implementing");
    let decisions = store.decisions_for(&workflow.id).await?;
    assert!(decisions
        .iter()
        .any(|record| record.decision.decision == "resume_implementation_after_replan"));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_persists_bind_pr_payload_for_pr_open_transition() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 123, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 123,
        "task_id": "task-123",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("implement_issue", "Implementation completed.")
            .with_artifact(ActivityArtifact::new(
                "pull_request",
                json!({
                    "pr_number": 77,
                    "pr_url": "https://github.com/owner/repo/pull/77",
                }),
            )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "pr_open");
    assert_eq!(reloaded.data["project_id"], "/project-a");
    assert_eq!(reloaded.data["issue_number"], 123);
    assert_eq!(reloaded.data["pr_number"], 77);
    assert_eq!(
        reloaded.data["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );

    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    assert_eq!(
        commands[1].command.command_type,
        WorkflowCommandType::BindPr
    );
    assert_ne!(commands[1].status, WorkflowCommandStatus::Pending);
    Ok(())
}

#[tokio::test]
async fn runtime_worker_blocks_invalid_inline_bind_pr_before_persisting_command(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 123, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 123,
        "task_id": "task-123",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let malformed_decision = WorkflowDecision::new(
        &workflow.id,
        "implementing",
        "agent_reported_pr_open",
        "pr_open",
        "The agent reported a PR with incomplete metadata.",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::BindPr,
        "issue-123-bind-pr",
        json!({ "pr_number": 77 }),
    ));
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("implement_issue", "Implementation completed.")
            .with_artifact(ActivityArtifact::new(
                "workflow_decision",
                serde_json::to_value(&malformed_decision).expect("decision should serialize"),
            )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "blocked");
    assert!(reloaded.data.get("pr_number").is_none());
    assert!(reloaded.data.get("pr_url").is_none());

    let commands = store.commands_for(&workflow.id).await?;
    assert!(commands.iter().all(|record| {
        record.command.command_type != WorkflowCommandType::BindPr
            && record.command.dedupe_key != "issue-123-bind-pr"
    }));
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::MarkBlocked
            && record.status == WorkflowCommandStatus::HandledInline
    }));

    let decisions = store.decisions_for(&workflow.id).await?;
    assert!(decisions.iter().any(|record| {
        record.accepted && record.decision.decision == "block_invalid_agent_output"
    }));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_blocks_implementation_success_without_pr_evidence() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 124, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 124,
        "task_id": "task-124",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-124-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded("implement_issue", "Implementation completed."),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "blocked");
    assert_eq!(reloaded.data["project_id"], "/project-a");
    assert_eq!(reloaded.data["issue_number"], 124);

    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands[0].id, command_id);
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::MarkBlocked
            && record.status == WorkflowCommandStatus::HandledInline
    }));
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::RequestOperatorAttention
            && record.status == WorkflowCommandStatus::HandledInline
    }));

    let decisions = store.decisions_for(&workflow.id).await?;
    assert!(decisions.iter().any(|record| {
        record.accepted && record.decision.decision == "block_missing_implementation_result"
    }));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_finishes_closed_issue_success_without_pr() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-a", 125, "implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 125,
        "task_id": "task-125",
    }));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "issue-125-implement");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(
            "implement_issue",
            "Issue was already resolved upstream.",
        )
        .with_signal(ActivitySignal::new(
            "IssueAlreadyResolved",
            json!({
                "issue_number": 125,
                "state": "closed",
                "issue_url": "https://github.com/owner/repo/issues/125",
            }),
        )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);

    let reloaded = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(reloaded.state, "done");
    assert_eq!(reloaded.data["project_id"], "/project-a");
    assert_eq!(reloaded.data["issue_number"], 125);
    assert_eq!(
        reloaded.data["closed_issue_evidence"]["issue_url"],
        "https://github.com/owner/repo/issues/125"
    );

    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands[0].id, command_id);
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    assert!(commands.iter().any(|record| {
        record.command.command_type == WorkflowCommandType::MarkDone
            && record.status == WorkflowCommandStatus::HandledInline
    }));

    let decisions = store.decisions_for(&workflow.id).await?;
    assert!(decisions
        .iter()
        .any(|record| record.accepted && record.decision.decision == "finish_closed_issue"));
    Ok(())
}

#[tokio::test]
async fn runtime_worker_propagates_pr_feedback_child_completion_to_parent() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child")
    .with_parent(parent.id.clone());
    store.upsert_instance(&child).await?;
    let command =
        WorkflowCommand::enqueue_activity(PR_FEEDBACK_INSPECT_ACTIVITY, "inspect-pr-feedback-77");
    let command_id = store.enqueue_command(&child.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": PR_FEEDBACK_INSPECT_ACTIVITY }),
        )
        .await?;
    let worker = RuntimeWorker::new(&store, "runtime-1").with_lease_ttl(Duration::minutes(5));
    let malformed_parent_decision = WorkflowDecision::new(
        &parent.id,
        "awaiting_feedback",
        "mark_ready_to_merge",
        "ready_to_merge",
        "Child inspection emitted a stale parent decision.",
    )
    .high_confidence();
    let executor = StaticRuntimeExecutor {
        result: ActivityResult::succeeded(
            PR_FEEDBACK_INSPECT_ACTIVITY,
            "PR feedback child found actionable feedback.",
        )
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(&malformed_parent_decision)?,
        ))
        .with_signal(ActivitySignal::new(
            "FeedbackFound",
            json!({ "pr_number": 77, "count": 1 }),
        )),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should claim and complete one job");

    assert_eq!(completed.id, job.id);
    let child_after = store
        .get_instance(&child.id)
        .await?
        .expect("child workflow should exist");
    assert_eq!(child_after.state, "feedback_found");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should exist");
    assert_eq!(parent_after.state, "addressing_feedback");
    let parent_commands = store.commands_for(&parent.id).await?;
    assert_eq!(parent_commands.len(), 1);
    assert_eq!(
        parent_commands[0].command.activity_name(),
        Some("address_pr_feedback")
    );
    let parent_events = store.events_for(&parent.id).await?;
    let propagated_event = parent_events
        .iter()
        .find(|event| {
            event.event_type == "RuntimeJobCompleted"
                && event.event["child_workflow_id"] == "pr-feedback-child"
        })
        .expect("child completion should propagate to parent");
    assert!(propagated_event.event["activity_result"]["artifacts"]
        .as_array()
        .expect("activity artifacts should be an array")
        .iter()
        .all(|artifact| artifact["artifact_type"] != "workflow_decision"));
    Ok(())
}

#[tokio::test]
async fn runtime_store_commits_parent_completion_event_decision_and_command() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-transaction")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Runtime child workflow found actionable PR feedback.",
    )
    .with_signal(ActivitySignal::new(
        "FeedbackFound",
        json!({ "pr_number": 77, "count": 1 }),
    ));

    let record = store
        .commit_parent_runtime_completion(
            &parent.id,
            "runtime-1",
            json!({
                "command_id": "child-command-1",
                "runtime_job_id": "child-job-1",
                "child_workflow_id": "pr-feedback-child-transaction",
                "activity_result": result,
            }),
        )
        .await?
        .expect("parent completion should produce a decision");

    assert!(record.accepted);
    assert_eq!(record.decision.decision, "address_pr_feedback");
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "addressing_feedback");
    let parent_events = store.events_for(&parent.id).await?;
    assert!(parent_events.iter().any(|event| {
        event.event_type == "RuntimeJobCompleted"
            && event.event["child_workflow_id"] == "pr-feedback-child-transaction"
    }));
    let parent_decisions = store.decisions_for(&parent.id).await?;
    assert_eq!(parent_decisions.len(), 1);
    assert_eq!(parent_decisions[0].id, record.id);
    let parent_commands = store.commands_for(&parent.id).await?;
    assert_eq!(parent_commands.len(), 1);
    assert_eq!(
        parent_commands[0].command.activity_name(),
        Some("address_pr_feedback")
    );
    assert_eq!(
        parent_commands[0].decision_id.as_deref(),
        Some(record.id.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_rolls_back_parent_completion_when_reducer_fails() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let parent = issue_instance("awaiting_feedback")
        .with_id("issue-parent-rollback")
        .with_data(json!({
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "runtime-task-77",
        }));
    store.upsert_instance(&parent).await?;

    let error = store
        .commit_parent_runtime_completion(
            &parent.id,
            "runtime-1",
            json!({
                "command_id": "child-command-rollback",
                "runtime_job_id": "child-job-rollback",
                "child_workflow_id": "pr-feedback-child-rollback",
                "activity_result": "not an activity result",
            }),
        )
        .await
        .expect_err("malformed activity_result should fail the parent completion transaction");

    assert!(
        error.to_string().contains("invalid type"),
        "unexpected error: {error:#}"
    );
    let parent_after = store
        .get_instance(&parent.id)
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "awaiting_feedback");
    assert!(store.events_for(&parent.id).await?.is_empty());
    assert!(store.decisions_for(&parent.id).await?.is_empty());
    assert!(store.commands_for(&parent.id).await?.is_empty());
    Ok(())
}
