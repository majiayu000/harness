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
    let instance = project_issue_instance("/project-a", 1567, "blocked");
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
        "last_stop": {
            "state": "blocked",
            "activity": "implement_issue",
            "runtime_job_id": runtime_job_id
        },
    }));
    store.upsert_instance(&instance).await?;

    let outcome = recover(
        &store,
        &instance.id,
        super::WorkflowRuntimeRecoveryAction::Unblock,
    )
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
    assert_operator_recovery_audit(
        &store,
        &instance.id,
        "WorkflowRuntimeUnblocked",
        "unblock",
        "blocked",
        "implementing",
    )
    .await?;
    Ok(())
}

#[rustfmt::skip]
#[tokio::test]
async fn runtime_recovery_unblocks_legacy_blocked_without_stop_metadata() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() { return Ok(()); }
    let dir = tempfile::tempdir()?; let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let legacy = project_issue_instance("/project-a", 1693, "blocked").with_data(json!({"project_id": "/project-a", "issue_number": 1693, "blocked_reason": "legacy blocked row without structured stop metadata", "source": "github"}));
    store.upsert_instance(&legacy).await?; let workflow = recovered_workflow(recover(&store, &legacy.id, super::WorkflowRuntimeRecoveryAction::Unblock).await?, "legacy unblock")?; assert_eq!(workflow.state, "implementing"); assert_eq!(workflow.data["blocked_reason"], legacy.data["blocked_reason"]); assert!(workflow.data.get("last_stop").is_none()); assert_operator_recovery_audit(&store, &legacy.id, "WorkflowRuntimeUnblocked", "unblock", "blocked", "implementing").await?;
    let commands = store.commands_for(&legacy.id).await?; assert_eq!(commands.len(), 1); assert_eq!(commands[0].status, WorkflowCommandStatus::Pending); let command = &commands[0].command; assert_eq!(command.command_type, WorkflowCommandType::EnqueueActivity); assert_eq!(command.activity_name(), Some("implement_issue")); assert_eq!(command.command["project_id"], "/project-a"); assert_eq!(command.command["issue_number"], 1693); assert_eq!(command.command["dispatch_gate"]["reason"], "operator_workflow_runtime_unblock"); assert!(command.command["additional_prompt"].as_str().is_some_and(|prompt| prompt.contains("Recovery reason: operator fixed transient failure"))); Ok(())
}

#[rustfmt::skip]
#[tokio::test]
async fn runtime_recovery_legacy_fallback_requires_absent_or_null_last_stop() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() { return Ok(()); } let dir = tempfile::tempdir()?; let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?; use super::WorkflowRuntimeRecoveryAction::{Retry, Unblock};
    for (issue_number, state, action, last_stop) in [(1710, "blocked", Unblock, None), (1711, "blocked", Unblock, Some(json!(null))), (1712, "failed", Retry, None), (1713, "failed", Retry, Some(json!(null)))] { let mut data = json!({"project_id": "/project-a", "issue_number": issue_number, "source": "github"}); if let Some(last_stop) = last_stop { data["last_stop"] = last_stop; } let original = project_issue_instance("/project-a", issue_number, state).with_data(data); store.upsert_instance(&original).await?; let workflow = recovered_workflow(recover(&store, &original.id, action).await?, "legacy absent/null recovery")?; assert_eq!(workflow.state, "implementing"); assert_eq!(workflow.data.get("last_stop"), original.data.get("last_stop")); let commands = store.commands_for(&original.id).await?; assert_eq!(commands.len(), 1); assert_eq!(commands[0].command.activity_name(), Some("implement_issue")); }
    for (issue_number, state, action, last_stop) in [(1720, "blocked", Unblock, json!({})), (1721, "blocked", Unblock, json!({"event_id": 123})), (1722, "blocked", Unblock, json!({"state": null, "activity": null, "runtime_job_id": null, "error_kind": null})), (1723, "failed", Retry, json!({})), (1724, "failed", Retry, json!({"event_id": 123})), (1725, "failed", Retry, json!({"state": null, "activity": null, "runtime_job_id": null, "error_kind": null}))] { let original = project_issue_instance("/project-a", issue_number, state).with_data(json!({"project_id": "/project-a", "issue_number": issue_number, "source": "github", "last_stop": last_stop})); store.upsert_instance(&original).await?; let outcome = recover(&store, &original.id, action).await?; assert!(matches!(outcome, super::WorkflowRuntimeRecoveryOutcome::UnsupportedStoppedActivity { activity: None, .. })); assert_recovery_left_workflow_unchanged(&store, &original).await?; }
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
            "start_child_workflow",
            "awaiting_feedback",
            WorkflowCommandType::StartChildWorkflow,
            pr_feedback_child_payload("pr:77", PR_FEEDBACK_INSPECT_ACTIVITY),
        ),
        (
            "address_pr_feedback",
            "addressing_feedback",
            WorkflowCommandType::EnqueueActivity,
            json!({"activity": "address_pr_feedback"}),
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
        let original = WorkflowCommand::new(
            command_type,
            format!("original-{stopped_activity}-{issue_number}"),
            payload.clone(),
        );
        let instance =
            store_stopped_failed_command(&store, issue_number, stopped_activity, &original).await?;

        let outcome = recover(
            &store,
            &instance.id,
            super::WorkflowRuntimeRecoveryAction::Retry,
        )
        .await?;
        let workflow = recovered_workflow(outcome, "lifecycle recovery")?;
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
        assert_operator_recovery_audit(
            &store,
            &instance.id,
            "WorkflowRuntimeRetried",
            "retry",
            "failed",
            expected_state,
        )
        .await?;
    }

    for (issue_number, bad_command) in [
        (
            1696,
            pr_feedback_child_command(
                "bad-sweep-child-activity",
                "pr:77",
                "wrong_activity",
            ),
        ),
        (
            1697,
            pr_feedback_child_command(
                "bad-sweep-child-subject",
                " ",
                PR_FEEDBACK_INSPECT_ACTIVITY,
            ),
        ),
    ] {
        let bad_child =
            store_stopped_failed_command(&store, issue_number, "start_child_workflow", &bad_command)
                .await?;
        let outcome = recover(
            &store,
            &bad_child.id,
            super::WorkflowRuntimeRecoveryAction::Retry,
        )
        .await?;
        assert_unsupported_activity(outcome, "start_child_workflow");
    }

    for (issue_number, state, action, data, field) in malformed_stop_metadata_cases() {
        let malformed = project_issue_instance("/project-a", issue_number, state).with_data(data);
        store.upsert_instance(&malformed).await?;
        let err = recover(
            &store,
            &malformed.id,
            action,
        )
        .await
            .expect_err("malformed stop metadata must fail recovery");
        assert!(err.to_string().contains(field), "{err}");
        assert_eq!(
            store.get_instance(&malformed.id).await?.unwrap().state,
            state
        );
        assert!(store.commands_for(&malformed.id).await?.is_empty());
    }

    let legacy = project_issue_instance("/project-a", 1698, "failed").with_data(json!({
        "project_id": "/project-a",
        "issue_number": 1698,
        "failure_reason": "legacy failed row without structured stop metadata",
    }));
    store.upsert_instance(&legacy).await?;
    let outcome = recover(
        &store,
        &legacy.id,
        super::WorkflowRuntimeRecoveryAction::Retry,
    )
    .await?;
    let workflow = recovered_workflow(outcome, "legacy recovery")?;
    assert_eq!(workflow.state, "implementing");
    let commands = store.commands_for(&legacy.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command.activity_name(), Some("implement_issue"));

    let unsupported = project_issue_instance("/project-a", 1699, "failed").with_data(json!({
        "error_kind": "timeout",
        "last_stop": {
            "state": "failed",
            "activity": "quality_gate",
            "runtime_job_id": "job-failed-1699",
            "error_kind": "timeout"
        },
    }));
    store.upsert_instance(&unsupported).await?;
    let outcome = recover(
        &store,
        &unsupported.id,
        super::WorkflowRuntimeRecoveryAction::Retry,
    )
    .await?;

    assert_unsupported_activity(outcome, "quality_gate");
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
    let instance = project_issue_instance("/project-a", 1568, "blocked");
    store.upsert_instance(&instance).await?;
    let original_command =
        WorkflowCommand::enqueue_activity("implement_issue", "stale-dispatched-1568");
    let (stale_command_id, runtime_job_id) =
        enqueue_original_runtime_job(&store, &instance.id, &original_command).await?;
    let instance = instance.with_data(json!({
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
        recover(
            &recovery_store,
            &recovery_workflow_id,
            super::WorkflowRuntimeRecoveryAction::Unblock,
        )
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

async fn recover(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    action: super::WorkflowRuntimeRecoveryAction,
) -> anyhow::Result<super::WorkflowRuntimeRecoveryOutcome> {
    store
        .recover_stopped_instance(super::WorkflowRuntimeRecoveryRequest {
            workflow_id,
            action,
            reason: "operator fixed transient failure",
            actor: "operator",
        })
        .await
}

fn recovered_workflow(
    outcome: super::WorkflowRuntimeRecoveryOutcome,
    context: &str,
) -> anyhow::Result<WorkflowInstance> {
    match outcome {
        super::WorkflowRuntimeRecoveryOutcome::Recovered { workflow, .. } => Ok(workflow),
        other => anyhow::bail!("expected {context}, got {other:?}"),
    }
}

fn assert_unsupported_activity(outcome: super::WorkflowRuntimeRecoveryOutcome, expected: &str) {
    assert!(matches!(outcome, super::WorkflowRuntimeRecoveryOutcome::UnsupportedStoppedActivity { activity: Some(activity), .. } if activity == expected));
}

#[rustfmt::skip]
fn malformed_stop_metadata_cases() -> [(u64, &'static str, super::WorkflowRuntimeRecoveryAction, serde_json::Value, &'static str); 6] {
    use super::WorkflowRuntimeRecoveryAction::{Retry, Unblock};
    [(1687, "blocked", Unblock, json!({"last_stop": 42}), "last_stop"), (1688, "blocked", Unblock, json!({"last_stop": {"state": "blocked", "runtime_job_id": 42}}), "last_stop.runtime_job_id"), (1689, "blocked", Unblock, json!({"last_stop": {"state": "blocked", "error_kind": "not_a_kind"}}), "last_stop.error_kind"), (1690, "failed", Retry, json!({"last_stop": 42}), "last_stop"), (1694, "failed", Retry, json!({"error_kind": "timeout", "last_stop": {"state": "failed", "activity": 42}}), "last_stop.activity"), (1695, "failed", Retry, json!({"last_stop": {"state": "failed", "error_kind": "not_a_kind"}}), "last_stop.error_kind")]
}

#[rustfmt::skip]
async fn assert_recovery_left_workflow_unchanged(store: &WorkflowRuntimeStore, original: &WorkflowInstance) -> anyhow::Result<()> {
    let stored = store.get_instance(&original.id).await?.expect("workflow should still exist"); assert_eq!(stored.state, original.state); assert_eq!(stored.data, original.data); assert!(store.events_for(&original.id).await?.is_empty()); assert!(store.decisions_for(&original.id).await?.is_empty()); assert!(store.commands_for(&original.id).await?.is_empty()); let (job_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM runtime_jobs AS job JOIN workflow_commands AS command ON command.id = job.command_id WHERE command.workflow_id = $1").bind(&original.id).fetch_one(store.pool()).await?; assert_eq!(job_count, 0); Ok(())
}

fn pr_feedback_child_command(
    dedupe_key: &str,
    subject_key: &str,
    child_activity: &str,
) -> WorkflowCommand {
    WorkflowCommand::new(
        WorkflowCommandType::StartChildWorkflow,
        dedupe_key,
        pr_feedback_child_payload(subject_key, child_activity),
    )
}

fn pr_feedback_child_payload(subject_key: &str, child_activity: &str) -> serde_json::Value {
    json!({
        "definition_id": PR_FEEDBACK_DEFINITION_ID,
        "child_activity": child_activity,
        "subject_key": subject_key,
        "pr_number": 77
    })
}

async fn store_stopped_failed_command(
    store: &WorkflowRuntimeStore,
    issue_number: u64,
    stopped_activity: &str,
    command: &WorkflowCommand,
) -> anyhow::Result<WorkflowInstance> {
    let instance = project_issue_instance("/project-a", issue_number, "failed");
    store.upsert_instance(&instance).await?;
    let (_command_id, runtime_job_id) =
        enqueue_original_runtime_job(store, &instance.id, command).await?;
    let instance = instance.with_data(json!({
        "error_kind": "timeout",
        "last_stop": {
            "state": "failed",
            "activity": stopped_activity,
            "runtime_job_id": runtime_job_id,
            "error_kind": "timeout"
        }
    }));
    store.upsert_instance(&instance).await?;
    Ok(instance)
}

async fn assert_operator_recovery_audit(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    event_type: &str,
    action: &str,
    previous_state: &str,
    state: &str,
) -> anyhow::Result<()> {
    let event = store
        .latest_event_for_type(workflow_id, event_type)
        .await?
        .expect("operator recovery event should be recorded");
    assert_eq!(event.source, "workflow_runtime_operator_action");
    let workflow = store
        .get_instance(workflow_id)
        .await?
        .expect("workflow should still exist");
    let recovery = &workflow.data["last_operator_recovery"];
    for (field, expected) in [
        ("action", action),
        ("previous_state", previous_state),
        ("state", state),
    ] {
        assert_eq!(event.event[field], expected);
        assert_eq!(recovery[field], expected);
    }
    assert_eq!(event.event["reason"], "operator fixed transient failure");
    assert_eq!(recovery["reason"], "operator fixed transient failure");
    assert_eq!(recovery["actor"], "operator");
    assert_eq!(recovery["event_id"], event.id);
    Ok(())
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
