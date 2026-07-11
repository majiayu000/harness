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
    let original_command =
        WorkflowCommand::enqueue_activity("implement_issue", "already-dispatched-1567");
    let (dispatched_id, runtime_job_id) =
        enqueue_original_runtime_job(&store, &instance.id, &original_command).await?;
    let instance = instance.with_data(json!({
        "project_id": "/project-a",
        "issue_number": 1567,
        "submission_mode": "immediate",
        "last_stop": {
            "state": "blocked",
            "activity": "implement_issue",
            "runtime_job_id": runtime_job_id
        },
    }));
    store.upsert_instance(&instance).await?;

    let outcome = store
        .recover_stopped_instance(super::WorkflowRuntimeRecoveryRequest {
            workflow_id: &instance.id,
            action: super::WorkflowRuntimeRecoveryAction::Unblock,
            reason: "operator supplied approval",
            actor: "operator",
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
async fn runtime_recovery_resumes_stopped_lifecycle_activity() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let cases = [
        (
            "merge_pr",
            "merging",
            WorkflowCommandType::EnqueueActivity,
            json!({"activity": "merge_pr", "approved_by": "dashboard", "pr_number": 77}),
        ),
        (
            "sweep_pr_feedback",
            "awaiting_feedback",
            WorkflowCommandType::StartChildWorkflow,
            json!({
                "definition_id": PR_FEEDBACK_DEFINITION_ID,
                "subject_key": "pr:77",
                "child_activity": PR_FEEDBACK_INSPECT_ACTIVITY,
                "pr_number": 77
            }),
        ),
        (
            "address_pr_feedback",
            "addressing_feedback",
            WorkflowCommandType::EnqueueActivity,
            json!({
                "activity": "address_pr_feedback",
                "source": "pr_hygiene",
                "pr_number": 77,
                "review_summary": "Gemini requested changes.",
                "hygiene": {"unresolved_threads": 2}
            }),
        ),
    ];

    for (index, (stopped_activity, expected_state, command_type, payload)) in cases.into_iter().enumerate() {
        let issue_number = 1700 + index as u64;
        let instance = project_issue_instance("/project-a", issue_number, "failed").with_data(json!({
            "project_id": "/project-a",
            "issue_number": issue_number,
            "error_kind": "timeout",
        }));
        store.upsert_instance(&instance).await?;
        let original = WorkflowCommand::new(
            command_type,
            format!("original-{stopped_activity}-{issue_number}"),
            payload.clone(),
        );
        let (_command_id, runtime_job_id) =
            enqueue_original_runtime_job(&store, &instance.id, &original).await?;
        let instance = instance.with_data(json!({
            "project_id": "/project-a",
            "issue_number": issue_number,
            "error_kind": "timeout",
            "last_stop": {
                "state": "failed",
                "activity": stopped_activity,
                "runtime_job_id": runtime_job_id,
                "error_kind": "timeout"
            }
        }));
        store.upsert_instance(&instance).await?;

        let outcome = store
            .recover_stopped_instance(super::WorkflowRuntimeRecoveryRequest {
                workflow_id: &instance.id,
                action: super::WorkflowRuntimeRecoveryAction::Retry,
                reason: "operator fixed transient failure",
                actor: "operator",
            })
            .await?;
        let workflow = match outcome {
            super::WorkflowRuntimeRecoveryOutcome::Recovered { workflow, .. } => workflow,
            other => anyhow::bail!("expected recovered outcome, got {other:?}"),
        };
        assert_eq!(workflow.state, expected_state);
        let commands = store.commands_for(&instance.id).await?;
        let recovery_command = commands
            .iter()
            .find(|command| command.status == WorkflowCommandStatus::Pending)
            .expect("recovery command should be pending");
        assert_eq!(recovery_command.command.command_type, command_type);
        assert_eq!(recovery_command.command.command, payload);
        assert_ne!(recovery_command.command.dedupe_key, original.dedupe_key);
        assert!(recovery_command
            .command
            .dedupe_key
            .starts_with("operator-recovery:retry:"));
        if stopped_activity == "address_pr_feedback" {
            assert_eq!(recovery_command.command.command["source"], "pr_hygiene");
            assert_eq!(
                recovery_command.command.command["review_summary"],
                "Gemini requested changes."
            );
            assert_eq!(
                recovery_command.command.command["hygiene"]["unresolved_threads"],
                2
            );
        }
    }

    let legacy = project_issue_instance("/project-a", 1698, "failed").with_data(json!({
        "project_id": "/project-a",
        "issue_number": 1698,
        "failure_reason": "legacy failed row without structured stop metadata",
    }));
    store.upsert_instance(&legacy).await?;
    let outcome = store
        .recover_stopped_instance(super::WorkflowRuntimeRecoveryRequest {
            workflow_id: &legacy.id,
            action: super::WorkflowRuntimeRecoveryAction::Retry,
            reason: "operator retried legacy failure",
            actor: "operator",
        })
        .await?;
    let workflow = match outcome {
        super::WorkflowRuntimeRecoveryOutcome::Recovered { workflow, .. } => workflow,
        other => anyhow::bail!("expected legacy recovery, got {other:?}"),
    };
    assert_eq!(workflow.state, "implementing");
    let commands = store.commands_for(&legacy.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command.activity_name(), Some("implement_issue"));

    let nonretryable = project_issue_instance("/project-a", 1697, "failed").with_data(json!({
        "error_kind": "configuration",
        "last_stop": {
            "state": "failed",
            "activity": "implement_issue",
            "error_kind": "configuration"
        }
    }));
    store.upsert_instance(&nonretryable).await?;
    let outcome = store
        .recover_stopped_instance(super::WorkflowRuntimeRecoveryRequest {
            workflow_id: &nonretryable.id,
            action: super::WorkflowRuntimeRecoveryAction::Retry,
            reason: "operator requested retry",
            actor: "operator",
        })
        .await?;
    assert!(matches!(
        outcome,
        super::WorkflowRuntimeRecoveryOutcome::NonRetryableFailure {
            error_kind: ActivityErrorKind::Configuration,
            ..
        }
    ));

    let unsupported = project_issue_instance("/project-a", 1699, "failed").with_data(json!({
        "project_id": "/project-a",
        "issue_number": 1699,
        "error_kind": "timeout",
        "last_stop": {
            "state": "failed",
            "activity": "quality_gate",
            "runtime_job_id": "job-failed-1699",
            "error_kind": "timeout"
        },
    }));
    store.upsert_instance(&unsupported).await?;
    let outcome = store
        .recover_stopped_instance(super::WorkflowRuntimeRecoveryRequest {
            workflow_id: &unsupported.id,
            action: super::WorkflowRuntimeRecoveryAction::Retry,
            reason: "operator fixed transient failure",
            actor: "operator",
        })
        .await?;

    assert!(matches!(
        outcome,
        super::WorkflowRuntimeRecoveryOutcome::UnsupportedStoppedActivity {
            activity: Some(activity),
            ..
        } if activity == "quality_gate"
    ));
    assert!(store.commands_for(&unsupported.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_recovery_waiting_on_command_does_not_lock_instance() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = Arc::new(WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?);
    let instance = project_issue_instance("/project-a", 1568, "blocked").with_data(json!({
        "project_id": "/project-a",
        "issue_number": 1568,
    }));
    store.upsert_instance(&instance).await?;
    let original_command =
        WorkflowCommand::enqueue_activity("implement_issue", "stale-dispatched-1568");
    let (stale_command_id, runtime_job_id) =
        enqueue_original_runtime_job(&store, &instance.id, &original_command).await?;
    let instance = instance.with_data(json!({
        "project_id": "/project-a",
        "issue_number": 1568,
        "last_stop": {
            "state": "blocked",
            "activity": "implement_issue",
            "runtime_job_id": runtime_job_id
        },
    }));
    store.upsert_instance(&instance).await?;

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

async fn enqueue_original_runtime_job(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    command: &WorkflowCommand,
) -> anyhow::Result<(String, String)> {
    let command_id = store.enqueue_command(workflow_id, None, command).await?;
    match store
        .enqueue_runtime_job_for_pending_command(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            command.command.clone(),
            None,
        )
        .await?
    {
        RuntimeJobEnqueueOutcome::Enqueued(job) => Ok((command_id, job.id)),
        other => anyhow::bail!("expected runtime job enqueue, got {other:?}"),
    }
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
