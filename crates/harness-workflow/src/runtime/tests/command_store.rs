#[tokio::test]
async fn runtime_store_can_insert_non_pending_command_atomically() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "scheduled");
    store.upsert_instance(&instance).await?;
    let command =
        WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement-inline");
    let command_id = store
        .enqueue_command_with_status(
            &instance.id,
            None,
            &command,
            WorkflowCommandStatus::HandledInline,
        )
        .await?;

    let commands = store.commands_for(&instance.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].id, command_id);
    assert_eq!(commands[0].status, WorkflowCommandStatus::HandledInline);
    assert!(
        store.pending_commands(10).await?.is_empty(),
        "inline commands must never be visible to the dispatcher as pending"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_non_pending_status_updates_existing_pending_command() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "scheduled");
    store.upsert_instance(&instance).await?;
    let command =
        WorkflowCommand::enqueue_activity("implement_issue", "issue-123-implement-inline");
    let pending_id = store.enqueue_command(&instance.id, None, &command).await?;
    let inline_id = store
        .enqueue_command_with_status(
            &instance.id,
            None,
            &command,
            WorkflowCommandStatus::HandledInline,
        )
        .await?;

    assert_eq!(inline_id, pending_id);
    let commands = store.commands_for(&instance.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, WorkflowCommandStatus::HandledInline);
    assert!(
        store.pending_commands(10).await?.is_empty(),
        "dedupe conflict must not leave an inline command visible as pending"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_dedupe_status_update_does_not_regress_dispatched_command(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "replanning");
    store.upsert_instance(&instance).await?;
    let command = WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-dispatched");
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    let outcome = store
        .enqueue_runtime_job_for_pending_command(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"activity": "replan_issue"}),
            None,
        )
        .await?;
    assert!(matches!(outcome, RuntimeJobEnqueueOutcome::Enqueued(_)));

    let duplicate_id = store
        .enqueue_command_with_status(
            &instance.id,
            None,
            &command,
            WorkflowCommandStatus::HandledInline,
        )
        .await?;

    assert_eq!(duplicate_id, command_id);
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        WorkflowCommandStatus::Dispatched
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_pending_command_enqueue_is_idempotent_across_concurrent_claims(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "replanning");
    store.upsert_instance(&instance).await?;
    let command = WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-idempotent");
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;

    let first = store.enqueue_runtime_job_for_pending_command(
        &command_id,
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({"activity": "replan_issue"}),
        None,
    );
    let second = store.enqueue_runtime_job_for_pending_command(
        &command_id,
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({"activity": "replan_issue"}),
        None,
    );
    let (first, second) = tokio::join!(first, second);
    let outcomes = [first?, second?];
    let enqueued: Vec<&RuntimeJobEnqueueOutcome> = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RuntimeJobEnqueueOutcome::Enqueued(_)))
        .collect();
    let already_exists: Vec<&RuntimeJobEnqueueOutcome> = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RuntimeJobEnqueueOutcome::AlreadyExists(_)))
        .collect();
    assert_eq!(enqueued.len(), 1);
    assert_eq!(already_exists.len(), 1);

    let first_job_id = match enqueued[0] {
        RuntimeJobEnqueueOutcome::Enqueued(job) => job.id.clone(),
        other => panic!("unexpected enqueue outcome: {other:?}"),
    };
    match already_exists[0] {
        RuntimeJobEnqueueOutcome::AlreadyExists(job) => assert_eq!(job.id, first_job_id),
        other => panic!("unexpected idempotent enqueue outcome: {other:?}"),
    }

    let third = store
        .enqueue_runtime_job_for_pending_command(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"activity": "replan_issue"}),
            None,
        )
        .await?;
    match third {
        RuntimeJobEnqueueOutcome::AlreadyExists(job) => assert_eq!(job.id, first_job_id),
        other => panic!("unexpected third enqueue outcome: {other:?}"),
    }

    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, first_job_id);
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        WorkflowCommandStatus::Dispatched
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_claims_pending_commands_with_dispatch_lease() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "replanning");
    store.upsert_instance(&instance).await?;
    let command = WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-lease");
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;

    let claimed = store
        .claim_pending_commands("dispatcher-a", Utc::now() + Duration::seconds(60), 10)
        .await?;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, command_id);
    assert_eq!(claimed[0].status, WorkflowCommandStatus::Dispatching);
    assert_eq!(claimed[0].dispatch_owner.as_deref(), Some("dispatcher-a"));
    assert!(claimed[0].dispatch_lease_expires_at.is_some());
    assert!(store.pending_commands(10).await?.is_empty());

    let concurrent = store
        .claim_pending_commands("dispatcher-b", Utc::now() + Duration::seconds(60), 10)
        .await?;
    assert!(
        concurrent.is_empty(),
        "an unexpired dispatch lease must hide the command from other dispatchers"
    );

    let stale_command =
        WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-stale-lease");
    let stale_command_id = store
        .enqueue_command(&instance.id, None, &stale_command)
        .await?;
    let stale_claim = store
        .claim_pending_commands("stale-dispatcher", Utc::now() - Duration::seconds(1), 10)
        .await?;
    assert_eq!(stale_claim.len(), 1);
    assert_eq!(stale_claim[0].id, stale_command_id);

    let reclaimed = store
        .claim_pending_commands("dispatcher-c", Utc::now() + Duration::seconds(60), 10)
        .await?;
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].id, stale_command_id);
    assert_eq!(reclaimed[0].dispatch_owner.as_deref(), Some("dispatcher-c"));
    Ok(())
}
