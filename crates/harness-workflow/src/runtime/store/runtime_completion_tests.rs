use super::*;
use crate::runtime::model::{WorkflowCommandType, WorkflowEvidence, WorkflowSubject};
use crate::runtime::{DataProvenance, PromptContinuationPolicy, RuntimeJobStatus, RuntimeKind};
use chrono::Utc;
use harness_core::db::resolve_database_url;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

fn pin_error_instance(id: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "missing_declarative_definition",
        42,
        "running",
        WorkflowSubject::new("test", id),
    )
    .with_id(id)
    .with_server_data(json!({
        "definition_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
    }))
}

fn pin_safety_decision(instance: &WorkflowInstance) -> WorkflowDecision {
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "definition_version_missing",
        "blocked",
        "pinned definition is unavailable",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkBlocked,
        "pin:block",
        json!({ "reason": "pinned definition is unavailable" }),
    ))
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::RequestOperatorAttention,
        "pin:operator",
        json!({ "reason": "pinned definition is unavailable" }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "definition_pin_error",
        "definition=missing_declarative_definition version=42 error=missing_version",
    ))
}

#[test]
fn completion_continuation_is_persisted_as_agent_data() -> anyhow::Result<()> {
    let policy = PromptContinuationPolicy {
        max_attempts: 3,
        attempt_delay_secs: 0,
        active_states: BTreeSet::from(["In Progress".to_string()]),
        no_progress_limit: 2,
    };
    let continuation = PromptContinuationState::initial(&policy);
    let mut instance = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("prompt", "continuation-agent"),
    )
    .with_classified_data(
        json!({ "prompt_ref": "prompt-ref" }),
        DataProvenance::Server,
    );

    persist_prompt_continuation(&mut instance, continuation)?;

    assert_eq!(
        instance
            .data_provenance
            .as_ref()
            .and_then(|sidecar| sidecar.provenance_for("/continuation")),
        Some(DataProvenance::Agent)
    );
    Ok(())
}

#[tokio::test]
async fn terminal_decision_cancels_pending_and_running_runtime_jobs() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("terminal-job-cleanup.db")).await?;
    let current = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("prompt", "terminal-job-cleanup"),
    )
    .with_id("terminal-job-cleanup")
    .with_server_data(json!({ "prompt_ref": "terminal-job-cleanup" }));
    store
        .force_upsert_lifecycle_state_for_test(&current)
        .await?;

    let first_command = store
        .enqueue_command(
            &current.id,
            None,
            &WorkflowCommand::enqueue_activity("first", "terminal:first"),
        )
        .await?;
    let second_command = store
        .enqueue_command(
            &current.id,
            None,
            &WorkflowCommand::enqueue_activity("second", "terminal:second"),
        )
        .await?;
    let first_job = store
        .enqueue_runtime_job(
            &first_command,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "first", "workflow_id": current.id }),
        )
        .await?;
    let second_job = store
        .enqueue_runtime_job(
            &second_command,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "second", "workflow_id": current.id }),
        )
        .await?;
    store
        .claim_next_runtime_job(
            "terminal-test-worker",
            Utc::now() + chrono::Duration::minutes(5),
        )
        .await?
        .expect("first runtime job should be claimed");

    let decision = WorkflowDecision::new(
        &current.id,
        "implementing",
        "finish",
        "done",
        "workflow completed",
    );
    let record = WorkflowDecisionRecord::accepted(decision, None);
    let mut target = current.clone();
    target.state = "done".to_string();
    target.version += 1;
    let mut tx = store.pool.begin().await?;
    insert_decision_record_once_tx(&mut tx, &record).await?;
    commit_decision_instance_tx(&mut tx, &current, &target, &record, false).await?;
    tx.commit().await?;

    for job_id in [&first_job.id, &second_job.id] {
        let job = store
            .get_runtime_job(job_id)
            .await?
            .expect("runtime job should remain auditable");
        assert_eq!(job.status, RuntimeJobStatus::Cancelled);
    }
    assert!(store
        .commands_for(&current.id)
        .await?
        .iter()
        .all(|command| command.status == WorkflowCommandStatus::Cancelled.as_str()));
    Ok(())
}

#[tokio::test]
async fn terminal_transition_fences_concurrent_command_and_job_enqueue() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store =
        Arc::new(WorkflowRuntimeStore::open(&dir.path().join("terminal-enqueue-fence.db")).await?);

    let command_workflow = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("prompt", "command-fence"),
    )
    .with_id("terminal-command-fence")
    .with_server_data(json!({ "prompt_ref": "command-fence" }));
    store
        .force_upsert_lifecycle_state_for_test(&command_workflow)
        .await?;
    let command_decision = WorkflowDecision::new(
        &command_workflow.id,
        "implementing",
        "finish",
        "done",
        "workflow completed",
    );
    let command_record = WorkflowDecisionRecord::accepted(command_decision, None);
    let mut command_target = command_workflow.clone();
    command_target.state = "done".to_string();
    command_target.version += 1;
    let mut terminal_tx = store.pool.begin().await?;
    select_instance_for_update_tx(&mut terminal_tx, &command_workflow.id)
        .await?
        .expect("workflow should be locked");
    insert_decision_record_once_tx(&mut terminal_tx, &command_record).await?;
    commit_decision_instance_tx(
        &mut terminal_tx,
        &command_workflow,
        &command_target,
        &command_record,
        false,
    )
    .await?;
    let enqueue_store = Arc::clone(&store);
    let workflow_id = command_workflow.id.clone();
    let command_enqueue = tokio::spawn(async move {
        enqueue_store
            .enqueue_command(
                &workflow_id,
                None,
                &WorkflowCommand::enqueue_activity("late", "terminal:late-command"),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !command_enqueue.is_finished(),
        "command enqueue must wait behind the terminal instance lock"
    );
    terminal_tx.commit().await?;
    let command_error = command_enqueue
        .await?
        .expect_err("command enqueue after terminal commit must fail");
    assert!(command_error.to_string().contains("terminal workflow"));

    let job_workflow = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("prompt", "job-fence"),
    )
    .with_id("terminal-job-fence")
    .with_server_data(json!({ "prompt_ref": "job-fence" }));
    store
        .force_upsert_lifecycle_state_for_test(&job_workflow)
        .await?;
    let command_id = store
        .enqueue_command(
            &job_workflow.id,
            None,
            &WorkflowCommand::enqueue_activity("late", "terminal:late-job"),
        )
        .await?;
    let job_decision = WorkflowDecision::new(
        &job_workflow.id,
        "implementing",
        "finish",
        "done",
        "workflow completed",
    );
    let job_record = WorkflowDecisionRecord::accepted(job_decision, None);
    let mut job_target = job_workflow.clone();
    job_target.state = "done".to_string();
    job_target.version += 1;
    let mut terminal_tx = store.pool.begin().await?;
    select_instance_for_update_tx(&mut terminal_tx, &job_workflow.id)
        .await?
        .expect("workflow should be locked");
    insert_decision_record_once_tx(&mut terminal_tx, &job_record).await?;
    commit_decision_instance_tx(
        &mut terminal_tx,
        &job_workflow,
        &job_target,
        &job_record,
        false,
    )
    .await?;
    let enqueue_store = Arc::clone(&store);
    let job_enqueue = tokio::spawn(async move {
        enqueue_store
            .enqueue_runtime_job(
                &command_id,
                RuntimeKind::CodexJsonrpc,
                "codex-default",
                json!({ "activity": "late" }),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !job_enqueue.is_finished(),
        "runtime job enqueue must wait behind the terminal instance lock"
    );
    terminal_tx.commit().await?;
    let job_error = job_enqueue
        .await?
        .expect_err("runtime job enqueue after terminal commit must fail");
    assert!(job_error.to_string().contains("terminal workflow"));
    Ok(())
}

#[tokio::test]
async fn pin_error_safety_decision_persists_blocked_without_current_definition(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("pin-safety.db")).await?;
    let instance = pin_error_instance("pin-safety-accepted");
    assert!(matches!(
        crate::runtime::state_registry::resolve_declarative_definition(&instance),
        crate::runtime::state_registry::DeclarativeDefinitionResolution::PinError(
            crate::runtime::state_registry::DeclarativeDefinitionPinError::MissingVersion
        )
    ));
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    let record = store
        .commit_runtime_completion_decision_for_test(
            &instance.id,
            "runtime-system",
            json!({}),
            &pin_safety_decision(&instance),
        )
        .await?
        .expect("decision should be recorded");
    assert!(record.accepted);
    assert_eq!(
        store
            .get_instance(&instance.id)
            .await?
            .expect("instance")
            .state,
        "blocked"
    );
    Ok(())
}

#[tokio::test]
async fn pin_error_safety_decision_requires_explicit_context_override() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("pin-safety-context.db")).await?;
    let instance = pin_error_instance("pin-safety-context-rejected");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    let mut tx = store.pool.begin().await?;
    let event = insert_event_tx(
        &mut tx,
        &instance.id,
        "RuntimeJobCompleted",
        "runtime-system",
        json!({}),
    )
    .await?;
    let record = persist_runtime_completion_decision_with_context_tx(
        &mut tx,
        instance.clone(),
        &event,
        pin_safety_decision(&instance),
        ValidationContext::new("runtime-system", event.created_at),
    )
    .await?;
    tx.commit().await?;

    assert!(!record.accepted);
    assert!(record
        .rejection_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("invalid declarative definition pin")));
    let persisted = store
        .get_instance(&instance.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("instance should remain present"))?;
    assert_eq!(persisted.state, "running");
    Ok(())
}

#[tokio::test]
async fn pin_error_safety_channel_rejects_any_extra_command() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("pin-safety-rejected.db")).await?;
    let instance = pin_error_instance("pin-safety-rejected");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    let decision = pin_safety_decision(&instance)
        .with_command(WorkflowCommand::wait("not allowed", "pin:extra"));
    let record = store
        .commit_runtime_completion_decision_for_test(
            &instance.id,
            "runtime-system",
            json!({}),
            &decision,
        )
        .await?
        .expect("decision should be recorded");
    assert!(!record.accepted);
    assert_eq!(
        store
            .get_instance(&instance.id)
            .await?
            .expect("instance")
            .state,
        "running"
    );
    Ok(())
}
