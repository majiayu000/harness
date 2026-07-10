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
async fn runtime_recovery_skips_superseded_active_commands() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 1567, "blocked").with_data(json!({
        "project_id": "/project-a",
        "issue_number": 1567,
        "submission_mode": "immediate",
    }));
    store.upsert_instance(&instance).await?;

    let pending_id = store
        .enqueue_command(
            &instance.id,
            None,
            &WorkflowCommand::enqueue_activity("implement_issue", "stale-pending-1567"),
        )
        .await?;
    let dispatching_id = store
        .enqueue_command(
            &instance.id,
            None,
            &WorkflowCommand::enqueue_activity("implement_issue", "stale-dispatching-1567"),
        )
        .await?;
    store
        .mark_command_status(&dispatching_id, WorkflowCommandStatus::Dispatching)
        .await?;
    let dispatched_id = store
        .enqueue_command(
            &instance.id,
            None,
            &WorkflowCommand::enqueue_activity("implement_issue", "already-dispatched-1567"),
        )
        .await?;
    let outcome = store
        .enqueue_runtime_job_for_pending_command(
            &dispatched_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"activity": "implement_issue"}),
            None,
        )
        .await?;
    assert!(matches!(outcome, RuntimeJobEnqueueOutcome::Enqueued(_)));

    let outcome = store
        .recover_stopped_instance(super::WorkflowRuntimeRecoveryRequest {
            workflow_id: &instance.id,
            action: super::WorkflowRuntimeRecoveryAction::Unblock,
            reason: "operator supplied approval",
            actor: "operator",
            next_state: "implementing",
        })
        .await?;
    assert!(matches!(
        outcome,
        super::WorkflowRuntimeRecoveryOutcome::Recovered { .. }
    ));

    let commands = store.commands_for(&instance.id).await?;
    assert_eq!(
        command_status(&commands, &pending_id),
        WorkflowCommandStatus::Skipped
    );
    assert_eq!(
        command_status(&commands, &dispatching_id),
        WorkflowCommandStatus::Skipped
    );
    assert_eq!(
        command_status(&commands, &dispatched_id),
        WorkflowCommandStatus::Cancelled
    );
    let jobs = store
        .runtime_jobs_for_commands(std::slice::from_ref(&dispatched_id))
        .await?;
    let dispatched_jobs = jobs
        .get(&dispatched_id)
        .expect("dispatched command should have a runtime job");
    assert_eq!(dispatched_jobs.len(), 1);
    assert_eq!(dispatched_jobs[0].status, RuntimeJobStatus::Cancelled);
    assert_eq!(dispatched_jobs[0].output.as_ref().unwrap()["status"], "cancelled");
    let pending_commands: Vec<_> = commands
        .iter()
        .filter(|command| command.status == WorkflowCommandStatus::Pending)
        .collect();
    assert_eq!(pending_commands.len(), 1);
    assert_eq!(pending_commands[0].command.activity_name(), Some("implement_issue"));
    assert!(pending_commands[0]
        .command
        .dedupe_key
        .starts_with("operator-recovery:unblock:"));

    let events = store.events_for(&instance.id).await?;
    let recovery_event = events
        .iter()
        .find(|event| event.event_type == "WorkflowRuntimeUnblocked")
        .expect("recovery audit event should be recorded");
    assert_eq!(recovery_event.event["superseded_command_count"], 3);
    assert_eq!(recovery_event.event["superseded_runtime_job_count"], 1);
    Ok(())
}

#[tokio::test]
async fn runtime_recovery_waiting_on_command_does_not_lock_instance() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = Arc::new(WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?);
    let instance = project_issue_instance("/project-a", 1568, "blocked");
    store.upsert_instance(&instance).await?;
    let stale_command_id = store
        .enqueue_command(
            &instance.id,
            None,
            &WorkflowCommand::enqueue_activity("implement_issue", "stale-pending-1568"),
        )
        .await?;

    let mut command_tx = store.pool().begin().await?;
    sqlx::query("SELECT id FROM workflow_commands WHERE id = $1 FOR UPDATE")
        .bind(&stale_command_id)
        .execute(&mut *command_tx)
        .await?;

    let recovery_store = Arc::clone(&store);
    let recovery_workflow_id = instance.id.clone();
    let recovery = tokio::spawn(async move {
        recovery_store
            .recover_stopped_instance(super::WorkflowRuntimeRecoveryRequest {
                workflow_id: &recovery_workflow_id,
                action: super::WorkflowRuntimeRecoveryAction::Unblock,
                reason: "operator supplied approval",
                actor: "operator",
                next_state: "implementing",
            })
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut instance_tx = store.pool().begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '250ms'")
        .execute(&mut *instance_tx)
        .await?;
    sqlx::query("SELECT data::text FROM workflow_instances WHERE id = $1 FOR UPDATE")
        .bind(&instance.id)
        .fetch_optional(&mut *instance_tx)
        .await
        .expect("recovery waiting on a command must not already hold the instance lock");
    instance_tx.rollback().await?;

    command_tx.rollback().await?;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), recovery).await???;
    assert!(matches!(
        outcome,
        super::WorkflowRuntimeRecoveryOutcome::Recovered { .. }
    ));
    Ok(())
}

fn command_status(commands: &[WorkflowCommandRecord], command_id: &str) -> WorkflowCommandStatus {
    commands
        .iter()
        .find(|command| command.id == command_id)
        .map(|command| command.status)
        .expect("command should exist")
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
