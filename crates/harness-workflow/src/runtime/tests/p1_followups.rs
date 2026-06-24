use super::*;

#[tokio::test]
async fn runtime_store_get_instance_by_pr_prefers_issue_bound_workflow() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let issue_bound = WorkflowInstance::new(
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
    let pr_only = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "awaiting_feedback",
        WorkflowSubject::new("pull_request", "pr:880"),
    )
    .with_id("project-a::owner/repo::pr:880")
    .with_data(json!({
        "project_id": "project-a",
        "repo": "owner/repo",
        "pr_number": 880,
    }));
    store.upsert_instance(&issue_bound).await?;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    store.upsert_instance(&pr_only).await?;

    let found = store
        .get_instance_by_pr("github_issue_pr", "project-a", Some("owner/repo"), 880)
        .await?
        .expect("matching runtime workflow should be found");
    assert_eq!(found.id, issue_bound.id);
    Ok(())
}

#[tokio::test]
async fn runtime_worker_completes_job_when_workflow_already_done() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = issue_instance("done");
    store.upsert_instance(&instance).await?;
    let job = enqueue_workflow_runtime_job(
        &store,
        &instance.id,
        "command-terminal-done",
        RuntimeKind::ClaudeCode,
        "claude-default",
        json!({ "activity": "implement_issue", "workflow_id": instance.id }),
        None,
    )
    .await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = RuntimeWorker::new(&store, "runtime-1");
    let executor = CountingRuntimeExecutor {
        result: ActivityResult::failed("implement_issue", "should not run", "unexpected call"),
        calls: calls.clone(),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should complete stale terminal job");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    let output: ActivityResult =
        serde_json::from_value(completed.output.expect("activity result output"))?;
    assert_eq!(output.status, ActivityStatus::Succeeded);
    assert!(output.summary.contains("already terminal (done)"));
    Ok(())
}

#[tokio::test]
async fn runtime_store_pending_dedupe_refreshes_command_payload() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = issue_instance("implementing").with_id("issue-dedupe-refresh");
    store.upsert_instance(&instance).await?;

    let first =
        WorkflowCommand::enqueue_activity("implement_issue", "issue:owner/repo:issue:1200:start");
    let mut old_decision = WorkflowDecisionRecord::accepted(
        WorkflowDecision::new(
            instance.id.clone(),
            "implementing",
            "enqueue_old_command",
            "implementing",
            "Record the original pending command.",
        ),
        None,
    );
    old_decision.id = "decision-old".to_string();
    store.record_decision(&old_decision).await?;
    let command_id = store
        .enqueue_command(&instance.id, Some("decision-old"), &first)
        .await?;
    let updated = WorkflowCommand::start_child_workflow(
        "github_issue_pr",
        "issue:1200",
        "issue:owner/repo:issue:1200:start",
    );
    let mut new_decision = WorkflowDecisionRecord::accepted(
        WorkflowDecision::new(
            instance.id.clone(),
            "implementing",
            "refresh_command_payload",
            "implementing",
            "Refresh the pending command payload.",
        ),
        None,
    );
    new_decision.id = "decision-new".to_string();
    store.record_decision(&new_decision).await?;
    let duplicate_id = store
        .enqueue_command(&instance.id, Some("decision-new"), &updated)
        .await?;

    assert_eq!(duplicate_id, command_id);
    let commands = store.commands_for(&instance.id).await?;
    assert_eq!(commands.len(), 1);
    let command = &commands[0];
    assert_eq!(command.id, command_id);
    assert_eq!(command.status, "pending");
    assert_eq!(command.decision_id.as_deref(), Some("decision-new"));
    assert_eq!(
        command.command.command_type,
        WorkflowCommandType::StartChildWorkflow
    );
    assert_eq!(command.command.command["definition_id"], "github_issue_pr");
    assert_eq!(command.command.command["subject_key"], "issue:1200");
    Ok(())
}

#[tokio::test]
async fn runtime_store_running_lease_match_accepts_renewed_generation() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    enqueue_test_runtime_job(
        &store,
        "command-renewed-lease",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "start_child_workflow" }),
    )
    .await?;
    let initial_expires_at = Utc::now() - Duration::seconds(1);
    let claimed = store
        .claim_next_runtime_job("runtime-1", initial_expires_at)
        .await?
        .expect("runtime job should be claimable");
    assert_eq!(claimed.lease_generation, 1);
    let renewed_expires_at = initial_expires_at + Duration::minutes(5);
    let renewed = store
        .extend_runtime_job_lease_if_owned(
            &claimed.id,
            "runtime-1",
            initial_expires_at,
            renewed_expires_at,
        )
        .await?
        .expect("runtime job lease should renew for the same owner");
    assert_eq!(renewed.lease_generation, claimed.lease_generation);

    assert!(
        store.runtime_job_matches_running_lease(&claimed).await?,
        "same-owner renewal should still match the original running job snapshot"
    );
    assert!(store.runtime_job_matches_running_lease(&renewed).await?);
    Ok(())
}

#[tokio::test]
async fn runtime_store_running_lease_match_rejects_expired_same_owner_reclaim() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    enqueue_test_runtime_job(
        &store,
        "command-stale-lease",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "start_child_workflow" }),
    )
    .await?;
    let expired_expires_at = Utc::now() - Duration::minutes(1);
    let stale_claim = store
        .claim_next_runtime_job("runtime-1", expired_expires_at)
        .await?
        .expect("runtime job should be claimable");
    assert_eq!(stale_claim.lease_generation, 1);
    let reclaimed = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .expect("expired running job should be reclaimable by the same owner name");
    assert_eq!(reclaimed.lease_generation, 2);

    assert!(
        !store
            .runtime_job_matches_running_lease(&stale_claim)
            .await?,
        "expired same-owner claim should not match after reclaim"
    );
    assert!(store.runtime_job_matches_running_lease(&reclaimed).await?);
    Ok(())
}
