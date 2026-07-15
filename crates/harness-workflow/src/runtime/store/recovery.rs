use super::runtime_completion::validator_for_instance;
use super::runtime_job_leases::delete_runtime_job_lease_receipts_tx;
use super::{
    apply_inline_command_side_effect, command_store, enum_str, insert_decision_record_tx,
    insert_event_tx, select_instance_for_update_tx, to_jsonb_string, upsert_instance_tx,
    WorkflowInstance, WorkflowRuntimeStore,
};
use crate::runtime::model::{
    ActivityErrorKind, ActivityResult, RuntimeJob, RuntimeJobStatus, WorkflowCommand,
    WorkflowCommandType, WorkflowDecision, WorkflowDecisionRecord, WorkflowEvidence,
};
use crate::runtime::pr_feedback::{
    LOCAL_REVIEW_ACTIVITY, PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY,
};
use crate::runtime::reducer::GITHUB_ISSUE_PR_DEFINITION_ID;
use crate::runtime::state_registry::{
    DeclarativeDefinitionPinError, DeclarativeDefinitionResolution, WorkflowProgressMode,
};
use crate::runtime::status::WorkflowCommandStatus;
use crate::runtime::validator::ValidationContext;
use anyhow::{bail, Context};
use serde_json::{json, Value};

#[path = "recovery_validation.rs"]
mod recovery_validation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRuntimeRecoveryAction {
    Unblock,
    Retry,
}

impl WorkflowRuntimeRecoveryAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unblock => "unblock",
            Self::Retry => "retry",
        }
    }

    fn expected_state(self) -> &'static str {
        match self {
            Self::Unblock => "blocked",
            Self::Retry => "failed",
        }
    }

    fn event_type(self) -> &'static str {
        match self {
            Self::Unblock => "WorkflowRuntimeUnblocked",
            Self::Retry => "WorkflowRuntimeRetried",
        }
    }
}

#[rustfmt::skip]
pub struct WorkflowRuntimeRecoveryRequest<'a> {
    pub workflow_id: &'a str, pub action: WorkflowRuntimeRecoveryAction, pub reason: &'a str, pub actor: &'a str, pub target_state: Option<&'a str>, pub evidence: &'a [WorkflowEvidence],
}

#[rustfmt::skip]
#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowRuntimeRecoveryOutcome {
    Recovered { workflow: WorkflowInstance, previous_state: String },
    WrongState { workflow: WorkflowInstance },
    NonRetryableFailure { workflow: WorkflowInstance, error_kind: ActivityErrorKind },
    UnsupportedStoppedActivity { workflow: WorkflowInstance, activity: Option<String> },
    UnsupportedDefinition { workflow: WorkflowInstance },
    InvalidDefinitionPin { workflow: WorkflowInstance, error: DeclarativeDefinitionPinError },
    OperatorRequired { workflow: WorkflowInstance },
    TargetRequired { workflow: WorkflowInstance },
    TargetNotAllowed { workflow: WorkflowInstance, target_state: String },
    MissingRequiredEvidence { workflow: WorkflowInstance, detail: String },
    NotFound,
}

#[rustfmt::skip]
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryDispatchTarget { state: String, activity: Option<String> }

#[rustfmt::skip]
#[derive(Debug, Clone, PartialEq)]
struct RecoveryDispatchPlan { target: RecoveryDispatchTarget, command_source: RecoveryDispatchCommandSource }

#[derive(Debug, Clone, PartialEq)]
enum RecoveryDispatchCommandSource {
    Replay(WorkflowCommand),
    LegacyFallback,
    DeclarativeProgress(WorkflowCommandType),
}

impl WorkflowRuntimeStore {
    pub async fn recover_stopped_instance(
        &self,
        request: WorkflowRuntimeRecoveryRequest<'_>,
    ) -> anyhow::Result<WorkflowRuntimeRecoveryOutcome> {
        let mut tx = self.pool.begin().await?;
        let Some(snapshot) = select_instance_tx(&mut tx, request.workflow_id).await? else {
            tx.commit().await?;
            return Ok(WorkflowRuntimeRecoveryOutcome::NotFound);
        };
        let declarative = !matches!(
            crate::runtime::state_registry::resolve_declarative_definition(&snapshot),
            DeclarativeDefinitionResolution::NotDeclarative
        );
        if let Some(outcome) = recovery_rejection(&snapshot, &request)? {
            if declarative {
                audit_recovery_rejection_tx(&mut tx, &snapshot, &request, "eligibility_rejected")
                    .await?;
            }
            tx.commit().await?;
            return Ok(outcome);
        }
        let plan = match recovery_dispatch_plan_tx(&mut tx, &snapshot, &request).await? {
            Ok(plan) => plan,
            Err(activity) => {
                if declarative {
                    audit_recovery_rejection_tx(
                        &mut tx,
                        &snapshot,
                        &request,
                        "target_driver_unavailable",
                    )
                    .await?;
                }
                tx.commit().await?;
                return Ok(unsupported_stopped_activity(&snapshot, activity));
            }
        };
        if declarative {
            if let Some(outcome) =
                recovery_validation::validate_request_tx(&mut tx, &snapshot, &request, &plan)
                    .await?
            {
                tx.commit().await?;
                return Ok(outcome);
            }
        }
        let (superseded_command_count, superseded_runtime_job_count) =
            skip_superseded_active_commands_tx(&mut tx, &snapshot.id).await?;
        let Some(mut instance) =
            select_instance_for_update_tx(&mut tx, request.workflow_id).await?
        else {
            tx.rollback().await?;
            return Ok(WorkflowRuntimeRecoveryOutcome::NotFound);
        };

        if let Some(outcome) = recovery_rejection(&instance, &request)? {
            tx.rollback().await?;
            return Ok(outcome);
        }
        if recovery_dispatch_plan_tx(&mut tx, &instance, &request).await? != Ok(plan.clone()) {
            tx.rollback().await?;
            return Ok(unsupported_stopped_activity(&instance, None));
        }
        let previous_state = instance.state.clone();

        let event = insert_event_tx(
            &mut tx,
            &instance.id,
            request.action.event_type(),
            "workflow_runtime_operator_action",
            json!({
                "action": request.action.as_str(),
                "reason": request.reason,
                "actor": request.actor,
                "previous_state": previous_state,
                "state": plan.target.state,
                "superseded_command_count": superseded_command_count,
                "superseded_runtime_job_count": superseded_runtime_job_count,
            }),
        )
        .await?;

        let decision = recovery_dispatch_decision(
            &instance,
            request.action,
            request.reason,
            &previous_state,
            &plan,
            &event.id,
            request.evidence,
        );
        let Some(validator) = validator_for_instance(&instance)? else {
            anyhow::bail!(
                "workflow runtime recovery cannot validate definition {}",
                instance.definition_id
            );
        };
        let validation_context = if instance.is_terminal() {
            ValidationContext::new("workflow_runtime_operator_action", event.created_at)
                .allow_terminal_reopen()
        } else {
            ValidationContext::new("workflow_runtime_operator_action", event.created_at)
        };
        validator.validate(&instance, &decision, &validation_context)?;
        let decision_record =
            WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id.clone()));
        insert_decision_record_tx(&mut tx, &decision_record).await?;
        for command in &decision.commands {
            let status = recovery_command_status(command);
            command_store::insert_tx(
                &mut tx,
                &instance.id,
                Some(&decision_record.id),
                command,
                status,
            )
            .await?;
            if status == WorkflowCommandStatus::HandledInline {
                apply_inline_command_side_effect(&mut instance, command)?;
            }
        }

        instance.state = plan.target.state.to_string();
        instance.version = instance.version.saturating_add(1);
        instance.lease = None;
        persist_operator_recovery_data(
            &mut instance,
            request.action,
            request.reason,
            request.actor,
            &previous_state,
            &plan.target.state,
            &event.id,
        );
        upsert_instance_tx(&mut tx, &instance).await?;
        tx.commit().await?;

        Ok(WorkflowRuntimeRecoveryOutcome::Recovered {
            workflow: instance,
            previous_state,
        })
    }
}

fn recovery_command_status(command: &WorkflowCommand) -> WorkflowCommandStatus {
    if command.requires_runtime_job() {
        WorkflowCommandStatus::Pending
    } else {
        WorkflowCommandStatus::HandledInline
    }
}

#[rustfmt::skip]
async fn select_instance_tx(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, workflow_id: &str) -> anyhow::Result<Option<WorkflowInstance>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT data::text FROM workflow_instances WHERE id = $1").bind(workflow_id).fetch_optional(&mut **tx).await?;
    row.map(|(data,)| serde_json::from_str(&data)).transpose().map_err(Into::into)
}

#[rustfmt::skip]
async fn audit_recovery_rejection_tx(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, instance: &WorkflowInstance, request: &WorkflowRuntimeRecoveryRequest<'_>, reason_code: &str) -> anyhow::Result<()> {
    insert_event_tx(tx, &instance.id, "WorkflowRuntimeRecoveryRejected", "workflow_runtime_operator_action", json!({ "action": request.action.as_str(), "actor": request.actor, "reason": request.reason, "reason_code": reason_code, "state": instance.state })).await?;
    Ok(())
}

fn recovery_rejection(
    instance: &WorkflowInstance,
    request: &WorkflowRuntimeRecoveryRequest<'_>,
) -> anyhow::Result<Option<WorkflowRuntimeRecoveryOutcome>> {
    match crate::runtime::state_registry::resolve_declarative_definition(instance) {
        DeclarativeDefinitionResolution::PinError(error) => {
            return Ok(Some(WorkflowRuntimeRecoveryOutcome::InvalidDefinitionPin {
                workflow: instance.clone(),
                error,
            }));
        }
        DeclarativeDefinitionResolution::Resolved(definition) => {
            return Ok(declarative_recovery_rejection(
                instance,
                request,
                &definition,
            ));
        }
        DeclarativeDefinitionResolution::NotDeclarative => {}
    }
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID {
        return Ok(Some(
            WorkflowRuntimeRecoveryOutcome::UnsupportedDefinition {
                workflow: instance.clone(),
            },
        ));
    }

    if instance.state != request.action.expected_state() {
        return Ok(Some(WorkflowRuntimeRecoveryOutcome::WrongState {
            workflow: instance.clone(),
        }));
    }

    if request.action == WorkflowRuntimeRecoveryAction::Retry {
        if let Some(error_kind) = stopped_error_kind(&instance.data)?.filter(|kind| {
            matches!(
                kind,
                ActivityErrorKind::Fatal | ActivityErrorKind::Configuration
            )
        }) {
            return Ok(Some(WorkflowRuntimeRecoveryOutcome::NonRetryableFailure {
                workflow: instance.clone(),
                error_kind,
            }));
        }
    }

    Ok(None)
}

#[rustfmt::skip]
fn declarative_recovery_rejection(instance: &WorkflowInstance, request: &WorkflowRuntimeRecoveryRequest<'_>, definition: &crate::runtime::declarative::DeclarativeWorkflowDefinition) -> Option<WorkflowRuntimeRecoveryOutcome> {
    if request.actor != "operator" { return Some(WorkflowRuntimeRecoveryOutcome::OperatorRequired { workflow: instance.clone() }); }
    if request.action != WorkflowRuntimeRecoveryAction::Unblock || instance.state != "blocked" { return Some(WorkflowRuntimeRecoveryOutcome::WrongState { workflow: instance.clone() }); }
    if request.target_state.is_none() && definition.policy().recovery_targets.len() != 1 { return Some(WorkflowRuntimeRecoveryOutcome::TargetRequired { workflow: instance.clone() }); }
    request.target_state.filter(|target| !definition.policy().recovery_targets.iter().any(|allowed| allowed == target)).map(|target_state| WorkflowRuntimeRecoveryOutcome::TargetNotAllowed { workflow: instance.clone(), target_state: target_state.to_string() })
}

async fn recovery_dispatch_plan_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
    request: &WorkflowRuntimeRecoveryRequest<'_>,
) -> anyhow::Result<Result<RecoveryDispatchPlan, Option<String>>> {
    if let DeclarativeDefinitionResolution::Resolved(definition) =
        crate::runtime::state_registry::resolve_declarative_definition(instance)
    {
        return declarative_recovery_dispatch_plan(request, &definition);
    }
    validate_stopped_metadata(&instance.data)?;
    let activity = stopped_activity(&instance.data)?;
    let target = match recovery_dispatch_target(&instance.data, activity.as_deref())? {
        Ok(target) => target,
        Err(activity) => return Ok(Err(activity)),
    };
    let command_source = if activity.is_some() {
        let Some(runtime_job_id) = stopped_runtime_job_id(&instance.data)? else {
            return Ok(Err(activity));
        };
        let command = select_command_for_runtime_job_tx(tx, &instance.id, &runtime_job_id)
            .await?
            .ok_or_else(|| activity.clone());
        let command = match command {
            Ok(command) => command,
            Err(activity) => return Ok(Err(activity)),
        };
        if !command_matches_recovery_target(&command, &target) {
            return Ok(Err(activity));
        }
        RecoveryDispatchCommandSource::Replay(command)
    } else {
        RecoveryDispatchCommandSource::LegacyFallback
    };
    Ok(Ok(RecoveryDispatchPlan {
        target: target.clone(),
        command_source,
    }))
}

fn declarative_recovery_dispatch_plan(
    request: &WorkflowRuntimeRecoveryRequest<'_>,
    definition: &crate::runtime::declarative::DeclarativeWorkflowDefinition,
) -> anyhow::Result<Result<RecoveryDispatchPlan, Option<String>>> {
    let target = request
        .target_state
        .unwrap_or_else(|| definition.policy().recovery_targets[0].as_str());
    let state = &definition.policy().states[target];
    let target = RecoveryDispatchTarget {
        state: target.to_string(),
        activity: state.activity.clone(),
    };
    let command_type = if state.activity.is_some() {
        WorkflowCommandType::EnqueueActivity
    } else {
        match definition
            .registered()
            .states
            .iter()
            .find(|candidate| candidate.key.state.as_ref() == target.state)
            .and_then(|candidate| candidate.progress_mode)
        {
            Some(WorkflowProgressMode::ExternalWait) => WorkflowCommandType::Wait,
            Some(WorkflowProgressMode::OperatorGate) => {
                WorkflowCommandType::RequestOperatorAttention
            }
            _ => return Ok(Err(None)),
        }
    };
    Ok(Ok(RecoveryDispatchPlan {
        target,
        command_source: RecoveryDispatchCommandSource::DeclarativeProgress(command_type),
    }))
}

fn recovery_dispatch_target(
    data: &Value,
    activity_name: Option<&str>,
) -> anyhow::Result<Result<RecoveryDispatchTarget, Option<String>>> {
    let activity = activity_name.map(ToOwned::to_owned);
    let Some(activity_name) = activity.as_deref() else {
        if has_no_structured_stop_metadata(data)? {
            return Ok(Ok(RecoveryDispatchTarget {
                state: "implementing".to_string(),
                activity: Some("implement_issue".to_string()),
            }));
        }
        return Ok(Err(activity));
    };
    let target = match activity_name {
        "implement_issue" => RecoveryDispatchTarget {
            state: "implementing".to_string(),
            activity: Some("implement_issue".to_string()),
        },
        "replan_issue" => RecoveryDispatchTarget {
            state: "replanning".to_string(),
            activity: Some("replan_issue".to_string()),
        },
        "merge_pr" => RecoveryDispatchTarget {
            state: "merging".to_string(),
            activity: Some("merge_pr".to_string()),
        },
        LOCAL_REVIEW_ACTIVITY => RecoveryDispatchTarget {
            state: "local_review_gate".to_string(),
            activity: Some(LOCAL_REVIEW_ACTIVITY.to_string()),
        },
        "sweep_pr_feedback" => RecoveryDispatchTarget {
            state: "awaiting_feedback".to_string(),
            activity: Some("sweep_pr_feedback".to_string()),
        },
        PR_FEEDBACK_INSPECT_ACTIVITY => RecoveryDispatchTarget {
            state: "awaiting_feedback".to_string(),
            activity: Some(PR_FEEDBACK_INSPECT_ACTIVITY.to_string()),
        },
        "start_child_workflow" => RecoveryDispatchTarget {
            state: "awaiting_feedback".to_string(),
            activity: Some("start_child_workflow".to_string()),
        },
        "address_pr_feedback" => RecoveryDispatchTarget {
            state: "addressing_feedback".to_string(),
            activity: Some("address_pr_feedback".to_string()),
        },
        _ => return Ok(Err(activity)),
    };
    Ok(Ok(target))
}

async fn select_command_for_runtime_job_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    runtime_job_id: &str,
) -> anyhow::Result<Option<WorkflowCommand>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT command.data::text FROM runtime_jobs AS job JOIN workflow_commands AS command ON command.id = job.command_id WHERE job.id = $1 AND command.workflow_id = $2",
    )
    .bind(runtime_job_id)
    .bind(workflow_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|(data,)| serde_json::from_str(&data))
        .transpose()
        .map_err(Into::into)
}

#[rustfmt::skip]
fn command_matches_recovery_target(command: &WorkflowCommand, target: &RecoveryDispatchTarget) -> bool {
    match command.command_type {
        WorkflowCommandType::EnqueueActivity => {
            command.activity_name() == target.activity.as_deref()
                && enqueue_payload_matches_target(&command.command)
        }
        WorkflowCommandType::StartChildWorkflow => {
            let payload = &command.command;
            matches!(target.activity.as_deref(), Some("start_child_workflow" | "sweep_pr_feedback"))
                && payload.get("definition_id").and_then(Value::as_str) == Some(PR_FEEDBACK_DEFINITION_ID)
                && payload.get("child_activity").and_then(Value::as_str) == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
                && payload.get("pr_number").and_then(Value::as_u64).is_some()
                && payload.get("subject_key").and_then(Value::as_str).is_some_and(|value| !value.trim().is_empty())
        }
        _ => false,
    }
}

fn enqueue_payload_matches_target(payload: &Value) -> bool {
    let review_summary = payload
        .get("review_summary")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    let hygiene = payload
        .get("hygiene")
        .or_else(|| payload.get("hygiene_context"))
        .is_some_and(|value| !value.is_null());
    payload.get("source").and_then(Value::as_str) != Some("pr_hygiene")
        || (payload.get("pr_number").and_then(Value::as_u64).is_some() && review_summary && hygiene)
}

#[rustfmt::skip]
fn unsupported_stopped_activity(instance: &WorkflowInstance, activity: Option<String>) -> WorkflowRuntimeRecoveryOutcome {
    WorkflowRuntimeRecoveryOutcome::UnsupportedStoppedActivity { workflow: instance.clone(), activity }
}

async fn skip_superseded_active_commands_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<(u64, u64)> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, status, data::text FROM workflow_commands WHERE workflow_id = $1 AND status IN ($2, $3, $4, $5) FOR UPDATE",
    )
    .bind(workflow_id)
    .bind(WorkflowCommandStatus::Pending.as_str())
    .bind(WorkflowCommandStatus::Dispatching.as_str())
    .bind(WorkflowCommandStatus::Dispatched.as_str())
    .bind(WorkflowCommandStatus::Deferred.as_str())
    .fetch_all(&mut **tx)
    .await?;

    let mut superseded_runtime_job_count = 0u64;
    for (command_id, status, data) in &rows {
        let command: WorkflowCommand = serde_json::from_str(data)?;
        let next_status = if status == WorkflowCommandStatus::Dispatched.as_str() {
            superseded_runtime_job_count += cancel_unfinished_runtime_jobs_tx(
                tx,
                command_id,
                command.runtime_activity_key(),
                "Workflow runtime operator recovery superseded this command.",
            )
            .await?;
            WorkflowCommandStatus::Cancelled
        } else {
            WorkflowCommandStatus::Skipped
        };
        sqlx::query(
            "UPDATE workflow_commands SET status = $2, dispatch_owner = NULL,
                dispatch_lease_expires_at = NULL, dispatch_not_before = NULL,
                dispatch_barrier = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = $1",
        )
        .bind(command_id)
        .bind(next_status.as_str())
        .execute(&mut **tx)
        .await?;
    }

    Ok((rows.len() as u64, superseded_runtime_job_count))
}

async fn cancel_unfinished_runtime_jobs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command_id: &str,
    activity: &str,
    summary: &str,
) -> anyhow::Result<u64> {
    let pending_status = enum_str(&RuntimeJobStatus::Pending)?;
    let running_status = enum_str(&RuntimeJobStatus::Running)?;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, data::text FROM runtime_jobs WHERE command_id = $1 AND status IN ($2, $3) FOR UPDATE",
    )
    .bind(command_id)
    .bind(&pending_status)
    .bind(&running_status)
    .fetch_all(&mut **tx)
    .await?;

    for (job_id, data) in &rows {
        let mut job: RuntimeJob = serde_json::from_str(data)?;
        job.complete(&ActivityResult::cancelled(activity, summary))?;
        let updated = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        sqlx::query(
            "UPDATE runtime_jobs SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP WHERE id = $4",
        )
        .bind(&status)
        .bind(job.not_before)
        .bind(&updated)
        .bind(job_id)
        .execute(&mut **tx)
        .await?;
        delete_runtime_job_lease_receipts_tx(tx, job_id, job.lease_generation).await?;
    }

    Ok(rows.len() as u64)
}

fn persist_operator_recovery_data(
    instance: &mut WorkflowInstance,
    action: WorkflowRuntimeRecoveryAction,
    reason: &str,
    actor: &str,
    previous_state: &str,
    state: &str,
    event_id: &str,
) {
    if !instance.data.is_object() {
        instance.data = json!({});
    }
    if let Some(data) = instance.data.as_object_mut() {
        // A successful recovery ends the stop episode: auto-recovery attempt
        // state (GH-1584) must not leak into a future stop episode.
        data.remove("auto_recovery");
        data.insert(
            "last_operator_recovery".to_string(),
            json!({
                "action": action.as_str(),
                "reason": reason,
                "actor": actor,
                "previous_state": previous_state,
                "state": state,
                "event_id": event_id,
            }),
        );
    }
}

fn recovery_dispatch_decision(
    instance: &WorkflowInstance,
    action: WorkflowRuntimeRecoveryAction,
    reason: &str,
    previous_state: &str,
    plan: &RecoveryDispatchPlan,
    event_id: &str,
    evidence: &[WorkflowEvidence],
) -> WorkflowDecision {
    let mut decision = WorkflowDecision::new(
        &instance.id,
        previous_state,
        format!("operator_runtime_{}", action.as_str()),
        &plan.target.state,
        format!(
            "operator requested workflow runtime {} after resolving the stopped condition",
            action.as_str()
        ),
    )
    .with_command(recovery_dispatch_command(
        instance, action, reason, plan, event_id,
    ));
    for item in evidence {
        decision = decision.with_evidence(item.clone());
    }
    decision
}

fn recovery_dispatch_command(
    instance: &WorkflowInstance,
    action: WorkflowRuntimeRecoveryAction,
    reason: &str,
    plan: &RecoveryDispatchPlan,
    event_id: &str,
) -> WorkflowCommand {
    let dedupe_key = format!(
        "operator-recovery:{}:{}:{}",
        action.as_str(),
        instance.id,
        event_id
    );
    if let RecoveryDispatchCommandSource::Replay(command) = &plan.command_source {
        let mut command = command.clone();
        command.dedupe_key = dedupe_key;
        return command;
    }
    if let RecoveryDispatchCommandSource::DeclarativeProgress(command_type) = plan.command_source {
        return WorkflowCommand::new(
            command_type,
            dedupe_key,
            json!({
                "reason": reason,
                "recovery_target": plan.target.state,
                "activity": plan.target.activity,
            }),
        );
    }

    let remote_fact_hash = optional_string_field(&instance.data, "last_remote_fact_hash");
    let dispatch_fact_hash = remote_fact_hash.clone();
    let mut payload = json!({
        "activity": plan.target.activity,
        "additional_prompt": format!(
            "Operator requested workflow runtime {} after resolving the stopped condition. Recovery reason: {}",
            action.as_str(),
            reason
        ),
        "dispatch_gate": {
            "reason": format!("operator_workflow_runtime_{}", action.as_str()),
            "fact_hash": dispatch_fact_hash,
        },
        "remote_fact_hash": remote_fact_hash,
        "submission_mode": optional_string_field(&instance.data, "submission_mode")
            .unwrap_or_else(|| "immediate".to_string()),
    });
    for field in RECOVERY_CONTEXT_FIELDS {
        copy_optional_data_field(&mut payload, &instance.data, field);
    }
    WorkflowCommand::new(WorkflowCommandType::EnqueueActivity, dedupe_key, payload)
}

#[rustfmt::skip]
const RECOVERY_CONTEXT_FIELDS: &[&str] = &["project_id", "repo", "issue_number", "pr_number", "pr_url", "task_id", "source", "external_id"];

fn stopped_error_kind(data: &Value) -> anyhow::Result<Option<ActivityErrorKind>> {
    let root = optional_error_kind(data.get("error_kind"), "error_kind")?;
    let last_stop = optional_error_kind(
        data.pointer("/last_stop/error_kind"),
        "last_stop.error_kind",
    )?;
    Ok(root.or(last_stop))
}

fn stopped_state(data: &Value) -> anyhow::Result<Option<String>> {
    optional_metadata_string(data.pointer("/last_stop/state"), "last_stop.state")
}

fn stopped_activity(data: &Value) -> anyhow::Result<Option<String>> {
    optional_metadata_string(data.pointer("/last_stop/activity"), "last_stop.activity")
}

#[rustfmt::skip]
fn stopped_runtime_job_id(data: &Value) -> anyhow::Result<Option<String>> {
    optional_metadata_string(data.pointer("/last_stop/runtime_job_id"), "last_stop.runtime_job_id")
}

#[rustfmt::skip]
fn validate_stopped_metadata(data: &Value) -> anyhow::Result<()> {
    if data.get("last_stop").filter(|value| !value.is_null()).is_some_and(|value| !value.is_object()) {
        bail!("workflow runtime recovery stop metadata `last_stop` must be an object");
    }
    let (_state, _activity, _runtime_job_id, _error_kind) = (stopped_state(data)?, stopped_activity(data)?, stopped_runtime_job_id(data)?, stopped_error_kind(data)?);
    Ok(())
}

#[rustfmt::skip]
fn has_no_structured_stop_metadata(data: &Value) -> anyhow::Result<bool> {
    Ok(data.get("last_stop").is_none_or(Value::is_null) && stopped_error_kind(data)?.is_none())
}

fn optional_metadata_string(value: Option<&Value>, field: &str) -> anyhow::Result<Option<String>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(value) = value.as_str() else {
        bail!("workflow runtime recovery stop metadata `{field}` must be a string");
    };
    let value = value.trim();
    if value.is_empty() {
        bail!("workflow runtime recovery stop metadata `{field}` must be non-empty");
    }
    Ok(Some(value.to_string()))
}

fn optional_error_kind(
    value: Option<&Value>,
    field: &str,
) -> anyhow::Result<Option<ActivityErrorKind>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    if value.as_str().is_some_and(|value| value.trim().is_empty()) {
        bail!("workflow runtime recovery stop metadata `{field}` must be non-empty");
    }
    serde_json::from_value(value.clone())
        .with_context(|| format!("workflow runtime recovery stop metadata `{field}` is invalid"))
        .map(Some)
}

fn optional_string_field(data: &Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn copy_optional_data_field(payload: &mut Value, data: &Value, field: &str) {
    let Some(value) = data.get(field).filter(|value| !value.is_null()) else {
        return;
    };
    if let Some(payload) = payload.as_object_mut() {
        payload.insert(field.to_string(), value.clone());
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
