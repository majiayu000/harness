use super::{data::eval_cleanup_data, transition_outcome::accepted_transition_record};
use crate::runtime::{
    DataProvenance, RuntimeJobStatus, ValidationContext, WorkflowCommand, WorkflowCommandStatus,
    WorkflowCommandType, WorkflowDecision, WorkflowDecisionTransition, WorkflowEvidence,
    WorkflowInstance, WorkflowRuntimeStore,
};
use chrono::Utc;
use serde_json::json;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

const REMOTE_CLEANUP_ACK_TIMEOUT: Duration = Duration::from_secs(30);
const REMOTE_CLEANUP_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub async fn cancel_eval_workflow_family(
    store: &WorkflowRuntimeStore,
    eval_run_id: &str,
    case_id: &str,
    root_workflow_id: &str,
    reason: &str,
) -> anyhow::Result<()> {
    let mut pending = vec![root_workflow_id.to_string()];
    let mut processed = BTreeSet::new();
    while let Some(workflow_id) = pending.pop() {
        if !processed.insert(workflow_id.clone()) {
            continue;
        }
        let children = store.list_instances_by_parent(&workflow_id, None).await?;
        pending.extend(children.into_iter().map(|child| child.id));
        for command in store.commands_for(&workflow_id).await? {
            if matches!(
                command.status,
                WorkflowCommandStatus::Pending
                    | WorkflowCommandStatus::Dispatching
                    | WorkflowCommandStatus::Deferred
                    | WorkflowCommandStatus::Dispatched
            ) {
                store
                    .cancel_command_and_unfinished_runtime_jobs(
                        &command.id,
                        command.command.runtime_activity_key(),
                        reason,
                    )
                    .await?;
            }
        }
        let Some(instance) = store.get_instance(&workflow_id).await? else {
            anyhow::bail!("eval workflow disappeared during cancellation: {workflow_id}");
        };
        if !instance.is_terminal() {
            cancel_workflow_instance(store, &instance, eval_run_id, case_id, reason).await?;
        }
        if pending.is_empty() {
            pending.extend(
                workflow_family_instances(store, root_workflow_id)
                    .await?
                    .into_iter()
                    .map(|instance| instance.id)
                    .filter(|workflow_id| !processed.contains(workflow_id)),
            );
        }
    }
    wait_for_cancelled_workflow_family_clean(store, root_workflow_id).await
}

async fn wait_for_cancelled_workflow_family_clean(
    store: &WorkflowRuntimeStore,
    root_workflow_id: &str,
) -> anyhow::Result<()> {
    let started = Instant::now();
    loop {
        match ensure_cancelled_workflow_family_clean(store, root_workflow_id).await {
            Ok(()) => return Ok(()),
            Err(error) if started.elapsed() >= REMOTE_CLEANUP_ACK_TIMEOUT => {
                return Err(
                    error.context("timed out waiting for remote eval cleanup acknowledgement")
                );
            }
            Err(_) => tokio::time::sleep(REMOTE_CLEANUP_POLL_INTERVAL).await,
        }
    }
}

async fn cancel_workflow_instance(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
    eval_run_id: &str,
    case_id: &str,
    reason: &str,
) -> anyhow::Result<()> {
    let observed_state = instance.state.clone();
    let mut final_instance = instance.clone();
    final_instance.state = "cancelled".to_string();
    final_instance.version = final_instance.version.saturating_add(1);
    final_instance.replace_classified_data(
        eval_cleanup_data(final_instance.data.clone(), eval_run_id, case_id, reason),
        DataProvenance::Server,
    );
    let decision = WorkflowDecision::new(
        &instance.id,
        &observed_state,
        "cancel_eval_workflow_family",
        "cancelled",
        reason,
    )
    .with_evidence(WorkflowEvidence::new(
        "eval_cleanup",
        format!("Eval run {eval_run_id} case {case_id} was cancelled."),
    ))
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkCancelled,
        format!(
            "eval-family-cleanup:{eval_run_id}:{case_id}:{}",
            instance.id
        ),
        json!({
            "reason": reason,
            "eval_run_id": eval_run_id,
            "case_id": case_id,
        }),
    ))
    .high_confidence();

    let validator = match instance.definition_id.as_str() {
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID => {
            crate::runtime::DecisionValidator::github_issue_pr()
        }
        crate::runtime::QUALITY_GATE_DEFINITION_ID => {
            crate::runtime::DecisionValidator::quality_gate()
        }
        crate::runtime::PR_FEEDBACK_DEFINITION_ID => {
            crate::runtime::DecisionValidator::pr_feedback()
        }
        crate::runtime::PROMPT_TASK_DEFINITION_ID => {
            crate::runtime::DecisionValidator::prompt_task()
        }
        definition_id => {
            anyhow::bail!("eval workflow family contains unsupported definition `{definition_id}`")
        }
    };
    validator.validate(
        instance,
        &decision,
        &ValidationContext::new("eval-cleanup", Utc::now()),
    )?;
    accepted_transition_record(
        store
            .apply_decision_transition(
                WorkflowDecisionTransition {
                    expected_state: &observed_state,
                    create_if_missing: None,
                    event_type: "EvalWorkflowFamilyCancelled",
                    source: "eval-cleanup",
                    payload: json!({
                        "eval_run_id": eval_run_id,
                        "case_id": case_id,
                        "reason": reason,
                    }),
                    decision: &decision,
                    final_instance: &final_instance,
                    command_status: WorkflowCommandStatus::Pending,
                },
                "eval-cleanup",
            )
            .await?,
        &instance.id,
        "eval family cleanup",
    )?
    .ok_or_else(|| anyhow::anyhow!("eval workflow changed during cancellation: {}", instance.id))?;
    Ok(())
}

pub async fn finalize_eval_case_cleanup(
    store: &WorkflowRuntimeStore,
    eval_run_id: &str,
    case_id: &str,
    root_workflow_id: &str,
) -> anyhow::Result<()> {
    if store
        .latest_event_for_type(root_workflow_id, "EvalCaseCleanupCompleted")
        .await?
        .is_some()
    {
        return Ok(());
    }

    let mut workflow_ids = Vec::new();
    let mut pending = vec![root_workflow_id.to_string()];
    while let Some(workflow_id) = pending.pop() {
        pending.extend(
            store
                .list_instances_by_parent(&workflow_id, None)
                .await?
                .into_iter()
                .map(|child| child.id),
        );
        workflow_ids.push(workflow_id);
    }

    ensure_completed_workflow_family_clean(store, root_workflow_id).await?;

    store
        .append_event(
            root_workflow_id,
            "EvalCaseCleanupCompleted",
            "eval-run",
            json!({
                "status": "cleaned",
                "eval_run_id": eval_run_id,
                "case_id": case_id,
                "workflow_ids": workflow_ids,
            }),
        )
        .await?;
    Ok(())
}

async fn ensure_cancelled_workflow_family_clean(
    store: &WorkflowRuntimeStore,
    root_workflow_id: &str,
) -> anyhow::Result<()> {
    let mut workflow_ids = Vec::new();
    let mut pending = vec![root_workflow_id.to_string()];
    while let Some(workflow_id) = pending.pop() {
        let Some(instance) = store.get_instance(&workflow_id).await? else {
            anyhow::bail!("eval workflow family member disappeared: {workflow_id}");
        };
        pending.extend(
            store
                .list_instances_by_parent(&workflow_id, None)
                .await?
                .into_iter()
                .map(|child| child.id),
        );
        if !instance.is_terminal() {
            anyhow::bail!(
                "workflow family still has nonterminal workflow {} ({})",
                instance.id,
                instance.state
            );
        }
        workflow_ids.push(workflow_id);
    }

    let commands = store.commands_for_workflows(&workflow_ids).await?;
    for command in commands.into_values().flatten() {
        if matches!(
            command.status,
            WorkflowCommandStatus::Pending
                | WorkflowCommandStatus::Dispatching
                | WorkflowCommandStatus::Deferred
                | WorkflowCommandStatus::Dispatched
        ) {
            anyhow::bail!(
                "workflow family still has active command {} ({})",
                command.id,
                command.status.as_str()
            );
        }
        for job in store.runtime_jobs_for_command(&command.id).await? {
            if matches!(
                job.status,
                RuntimeJobStatus::Pending | RuntimeJobStatus::Running
            ) {
                anyhow::bail!(
                    "workflow family still has active runtime job {} ({})",
                    job.id,
                    format!("{:?}", job.status).to_ascii_lowercase()
                );
            }
            if eval_job_requires_cleanup_proof(&job) && !runtime_job_has_cleanup_proof(&job) {
                anyhow::bail!(
                    "cancelled eval runtime job {} has no remote isolation cleanup proof",
                    job.id
                );
            }
        }
    }

    Ok(())
}

async fn ensure_completed_workflow_family_clean(
    store: &WorkflowRuntimeStore,
    root_workflow_id: &str,
) -> anyhow::Result<()> {
    let instances = workflow_family_instances(store, root_workflow_id).await?;
    let workflow_ids = instances
        .iter()
        .map(|instance| instance.id.clone())
        .collect::<Vec<_>>();
    ensure_no_active_work(store, &workflow_ids).await?;

    let commands = store.commands_for_workflows(&workflow_ids).await?;
    let mut required_cleanup_proofs = 0_u64;
    for command in commands.into_values().flatten() {
        for job in store.runtime_jobs_for_command(&command.id).await? {
            if eval_job_requires_cleanup_proof(&job) {
                required_cleanup_proofs = required_cleanup_proofs.saturating_add(1);
                if !runtime_job_has_cleanup_proof(&job) {
                    anyhow::bail!(
                        "eval runtime job {} has no remote isolation cleanup proof",
                        job.id
                    );
                }
            }
        }
    }
    if required_cleanup_proofs == 0 {
        anyhow::bail!("workflow family has no claimed eval runtime job cleanup evidence");
    }
    Ok(())
}

fn eval_job_requires_cleanup_proof(job: &crate::runtime::RuntimeJob) -> bool {
    job.runtime_kind == crate::runtime::RuntimeKind::RemoteHost
        && job.lease_generation > 0
        && (job.input.get("eval").is_some() || job.input.pointer("/command/eval").is_some())
}

fn runtime_job_has_cleanup_proof(job: &crate::runtime::RuntimeJob) -> bool {
    job.output
        .as_ref()
        .and_then(|output| {
            serde_json::from_value::<crate::runtime::ActivityResult>(output.clone()).ok()
        })
        .is_some_and(|result| {
            result.artifacts.iter().any(|artifact| {
                artifact.artifact_type
                    == crate::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP
                    && artifact
                        .artifact
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        == Some("cleaned")
            })
        })
}

async fn workflow_family_instances(
    store: &WorkflowRuntimeStore,
    root_workflow_id: &str,
) -> anyhow::Result<Vec<WorkflowInstance>> {
    let mut instances = Vec::new();
    let mut pending = vec![root_workflow_id.to_string()];
    while let Some(workflow_id) = pending.pop() {
        let Some(instance) = store.get_instance(&workflow_id).await? else {
            anyhow::bail!("eval workflow family member disappeared: {workflow_id}");
        };
        pending.extend(
            store
                .list_instances_by_parent(&workflow_id, None)
                .await?
                .into_iter()
                .map(|child| child.id),
        );
        instances.push(instance);
    }
    Ok(instances)
}

async fn ensure_no_active_work(
    store: &WorkflowRuntimeStore,
    workflow_ids: &[String],
) -> anyhow::Result<()> {
    let commands = store.commands_for_workflows(workflow_ids).await?;
    for command in commands.into_values().flatten() {
        if matches!(
            command.status,
            WorkflowCommandStatus::Pending
                | WorkflowCommandStatus::Dispatching
                | WorkflowCommandStatus::Deferred
                | WorkflowCommandStatus::Dispatched
        ) {
            anyhow::bail!(
                "workflow family still has active command {} ({})",
                command.id,
                command.status.as_str()
            );
        }
        for job in store.runtime_jobs_for_command(&command.id).await? {
            if matches!(
                job.status,
                RuntimeJobStatus::Pending | RuntimeJobStatus::Running
            ) {
                anyhow::bail!(
                    "workflow family still has active runtime job {} ({:?})",
                    job.id,
                    job.status
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        ActivityArtifact, ActivityResult, RuntimeJobCompletionLease, RuntimeKind, WorkflowSubject,
        GITHUB_ISSUE_PR_DEFINITION_ID,
    };

    #[test]
    fn claimed_eval_remote_job_requires_cleaned_isolation_proof() -> anyhow::Result<()> {
        let mut job = crate::runtime::RuntimeJob::pending(
            "command-1",
            RuntimeKind::RemoteHost,
            "eval-host",
            json!({"command": {"eval": {"eval_run_id": "run-1", "case_id": "case-1"}}}),
        );
        assert!(!eval_job_requires_cleanup_proof(&job));
        job.claim("host-1", Utc::now() + chrono::TimeDelta::minutes(5));
        assert!(eval_job_requires_cleanup_proof(&job));
        job.complete(&ActivityResult::cancelled("implement_issue", "cancelled"))?;
        assert!(!runtime_job_has_cleanup_proof(&job));

        let cleaned = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new(
                crate::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP,
                json!({"status": "cleaned"}),
            ),
        );
        job.complete(&cleaned)?;
        assert!(runtime_job_has_cleanup_proof(&job));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_marker_is_persisted_as_completed() -> anyhow::Result<()> {
        if harness_core::config::process_env::var_os("HARNESS_DATABASE_URL").is_none() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:42"),
        )
        .with_id("eval:run-cancel:case-1")
        .with_server_data(json!({
            "eval": {"eval_run_id": "run-cancel", "case_id": "case-1"},
        }));
        store
            .force_upsert_lifecycle_state_for_test(&workflow)
            .await?;

        cancel_eval_workflow_family(
            &store,
            "run-cancel",
            "case-1",
            &workflow.id,
            "operator cancelled",
        )
        .await?;

        let marker = store
            .commands_for(&workflow.id)
            .await?
            .into_iter()
            .find(|command| command.command.command_type == WorkflowCommandType::MarkCancelled)
            .ok_or_else(|| anyhow::anyhow!("cancellation marker is missing"))?;
        assert_eq!(marker.status, WorkflowCommandStatus::HandledInline);
        Ok(())
    }

    #[tokio::test]
    async fn completed_eval_allows_ready_to_merge_and_retained_draft_pr() -> anyhow::Result<()> {
        if harness_core::config::process_env::var_os("HARNESS_DATABASE_URL").is_none() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "ready_to_merge",
            WorkflowSubject::new("issue", "issue:42"),
        )
        .with_id("eval:run-success:case-1")
        .with_server_data(json!({
            "pr_number": 42,
            "eval": {"eval_run_id": "run-success", "case_id": "case-1"},
        }));
        store
            .force_upsert_lifecycle_state_for_test(&workflow)
            .await?;
        let command_id = store
            .enqueue_command(
                &workflow.id,
                None,
                &WorkflowCommand::enqueue_activity("implement_issue", "eval-success"),
            )
            .await?;
        store
            .enqueue_runtime_job(
                &command_id,
                RuntimeKind::RemoteHost,
                "eval-host",
                json!({
                    "activity": "implement_issue",
                    "command": {"eval": {"eval_run_id": "run-success", "case_id": "case-1"}},
                }),
            )
            .await?;
        let expires_at = Utc::now() + chrono::TimeDelta::minutes(5);
        let claimed = store
            .claim_next_runtime_job_for_runtime_kind(
                RuntimeKind::RemoteHost,
                "test-owner",
                expires_at,
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("runtime job should be claimable"))?;
        let result = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new(
                crate::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP,
                json!({"status": "cleaned"}),
            ),
        );
        let lease_proof = store
            .remote_runtime_job_lease_proof(
                &claimed.id,
                "test-owner",
                claimed.lease_generation,
                expires_at,
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("remote lease proof should be issued"))?;
        store
            .commit_runtime_activity_completion_if_owned_with_generation(
                &claimed.id,
                RuntimeJobCompletionLease::remote(
                    "test-owner",
                    expires_at,
                    claimed.lease_generation,
                    Some(lease_proof),
                ),
                &result,
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("runtime job should complete"))?;
        store
            .mark_command_status(&command_id, WorkflowCommandStatus::Completed)
            .await?;

        finalize_eval_case_cleanup(&store, "run-success", "case-1", &workflow.id).await?;

        let event = store
            .latest_event_for_type(&workflow.id, "EvalCaseCleanupCompleted")
            .await?
            .ok_or_else(|| anyhow::anyhow!("cleanup event should be persisted"))?;
        assert_eq!(event.event["status"], "cleaned");
        Ok(())
    }
}
