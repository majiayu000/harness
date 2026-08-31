use super::runtime_completion::validator_for_instance;
use super::runtime_job_state::{
    cancel_unfinished_runtime_jobs_for_commands_tx, RuntimeJobCancellation,
};
use super::{
    apply_inline_command_side_effect, command_store, commit_decision_instance_tx,
    insert_decision_record_once_tx, insert_event_tx, select_instance_for_update_tx,
    workflow_instance_from_persisted_json, WorkflowInstance, WorkflowRuntimeStore,
};
use crate::runtime::model::{
    ActivityErrorKind, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowDecisionRecord, WorkflowEvidence,
};
use crate::runtime::reducer::GITHUB_ISSUE_PR_DEFINITION_ID;
use crate::runtime::state_registry::{
    DeclarativeDefinitionPinError, DeclarativeDefinitionResolution, WorkflowDefinitionRegistry,
};
use crate::runtime::status::WorkflowCommandStatus;
use crate::runtime::validator::ValidationContext;
use anyhow::{bail, Context};
use serde_json::{json, Value};

#[path = "recovery_definition.rs"]
mod recovery_definition;
#[path = "recovery_validation.rs"]
mod recovery_validation;
use recovery_definition::{
    custom_declarative_definition, declarative_recovery_rejection, is_builtin_definition_id,
};
#[path = "recovery_dispatch.rs"]
mod recovery_dispatch;
use recovery_dispatch::*;
#[path = "recovery_declarative_plan.rs"]
mod recovery_declarative_plan;
use recovery_declarative_plan::declarative_recovery_dispatch_plan;

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
        let declarative =
            custom_declarative_definition(&self.definition_registry, &snapshot).is_some();
        if let Some(outcome) = recovery_rejection(&self.definition_registry, &snapshot, &request)? {
            if declarative {
                audit_recovery_rejection_tx(&mut tx, &snapshot, &request, "eligibility_rejected")
                    .await?;
            }
            tx.commit().await?;
            return Ok(outcome);
        }
        let plan = match recovery_dispatch_plan_tx(
            &mut tx,
            &self.definition_registry,
            &snapshot,
            &request,
        )
        .await?
        {
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
            if let Some(outcome) = recovery_validation::validate_request_tx(
                &mut tx,
                &self.definition_registry,
                &snapshot,
                &request,
                &plan,
            )
            .await?
            {
                tx.commit().await?;
                return Ok(outcome);
            }
        }
        let Some(mut instance) =
            select_instance_for_update_tx(&mut tx, request.workflow_id).await?
        else {
            tx.rollback().await?;
            return Ok(WorkflowRuntimeRecoveryOutcome::NotFound);
        };

        if let Some(outcome) = recovery_rejection(&self.definition_registry, &instance, &request)? {
            tx.rollback().await?;
            return Ok(outcome);
        }
        if recovery_dispatch_plan_tx(&mut tx, &self.definition_registry, &instance, &request)
            .await?
            != Ok(plan.clone())
        {
            tx.rollback().await?;
            return Ok(unsupported_stopped_activity(&instance, None));
        }
        let current = instance.clone();
        let (superseded_command_count, superseded_runtime_job_count) =
            skip_superseded_active_commands_tx(&mut tx, &instance.id).await?;
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
        let Some(validator) = validator_for_instance(&self.definition_registry, &instance)? else {
            anyhow::bail!(
                "workflow runtime recovery cannot validate definition {}",
                instance.definition_id
            );
        };
        let validation_context = if instance.is_terminal_with_registry(&self.definition_registry) {
            ValidationContext::new("workflow_runtime_operator_action", event.created_at)
                .allow_terminal_reopen()
        } else {
            ValidationContext::new("workflow_runtime_operator_action", event.created_at)
        };
        validator.validate(&instance, &decision, &validation_context)?;
        let decision_record =
            WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id.clone()));
        insert_decision_record_once_tx(&mut tx, &decision_record).await?;
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
        )?;
        commit_decision_instance_tx(&mut tx, &current, &instance, &decision_record, false).await?;
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
    row.map(|(data,)| workflow_instance_from_persisted_json(&data)).transpose()
}

#[rustfmt::skip]
async fn audit_recovery_rejection_tx(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, instance: &WorkflowInstance, request: &WorkflowRuntimeRecoveryRequest<'_>, reason_code: &str) -> anyhow::Result<()> {
    insert_event_tx(tx, &instance.id, "WorkflowRuntimeRecoveryRejected", "workflow_runtime_operator_action", json!({ "action": request.action.as_str(), "actor": request.actor, "reason": request.reason, "reason_code": reason_code, "state": instance.state })).await?;
    Ok(())
}

fn recovery_rejection(
    registry: &WorkflowDefinitionRegistry,
    instance: &WorkflowInstance,
    request: &WorkflowRuntimeRecoveryRequest<'_>,
) -> anyhow::Result<Option<WorkflowRuntimeRecoveryOutcome>> {
    match custom_declarative_definition(registry, instance) {
        Some(Ok(definition)) => {
            return Ok(declarative_recovery_rejection(
                instance,
                request,
                &definition,
            ))
        }
        Some(Err(error)) => {
            return Ok(Some(WorkflowRuntimeRecoveryOutcome::InvalidDefinitionPin {
                workflow: instance.clone(),
                error,
            }));
        }
        None => {}
    }
    if let DeclarativeDefinitionResolution::PinError(error) =
        registry.resolve_declarative_definition(instance)
    {
        if !is_builtin_definition_id(&instance.definition_id) {
            return Ok(Some(WorkflowRuntimeRecoveryOutcome::InvalidDefinitionPin {
                workflow: instance.clone(),
                error,
            }));
        }
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

async fn recovery_dispatch_plan_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    registry: &WorkflowDefinitionRegistry,
    instance: &WorkflowInstance,
    request: &WorkflowRuntimeRecoveryRequest<'_>,
) -> anyhow::Result<Result<RecoveryDispatchPlan, Option<String>>> {
    if let Some(Ok(definition)) = custom_declarative_definition(registry, instance) {
        return declarative_recovery_dispatch_plan(request, &definition, instance);
    }
    validate_stopped_metadata(&instance.data)?;
    let activity = stopped_activity(&instance.data)?;
    let mut target = match recovery_dispatch_target(&instance.data, activity.as_deref())? {
        Ok(target) => target,
        Err(activity) => return Ok(Err(activity)),
    };
    let command_source = if activity.is_some() {
        let Some(runtime_job_id) = stopped_runtime_job_id(&instance.data)? else {
            return Ok(Err(activity));
        };
        let direct_command =
            select_command_for_runtime_job_tx(tx, &instance.id, &runtime_job_id).await?;
        let command = match direct_command {
            Some(command) => Ok(command),
            None => {
                let parent_command =
                    select_parent_command_for_child_job_tx(tx, &instance.id, &runtime_job_id)
                        .await?;
                if parent_command.is_some() {
                    target = match recovery_dispatch_target(
                        &instance.data,
                        Some("start_child_workflow"),
                    )? {
                        Ok(target) => target,
                        Err(activity) => return Ok(Err(activity)),
                    };
                }
                parent_command.ok_or_else(|| activity.clone())
            }
        };
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

#[rustfmt::skip]
fn unsupported_stopped_activity(instance: &WorkflowInstance, activity: Option<String>) -> WorkflowRuntimeRecoveryOutcome {
    WorkflowRuntimeRecoveryOutcome::UnsupportedStoppedActivity { workflow: instance.clone(), activity }
}

async fn skip_superseded_active_commands_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<(u64, u64)> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, status, data::text FROM workflow_commands
         WHERE workflow_id = $1 AND status IN ($2, $3, $4, $5)
         ORDER BY id
         FOR UPDATE",
    )
    .bind(workflow_id)
    .bind(WorkflowCommandStatus::Pending.as_str())
    .bind(WorkflowCommandStatus::Dispatching.as_str())
    .bind(WorkflowCommandStatus::Dispatched.as_str())
    .bind(WorkflowCommandStatus::Deferred.as_str())
    .fetch_all(&mut **tx)
    .await?;

    let commands = rows
        .into_iter()
        .map(|(command_id, status, data)| {
            Ok((
                command_id,
                status,
                serde_json::from_str::<WorkflowCommand>(&data)?,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let cancellations = commands
        .iter()
        .filter(|(_, status, _)| status == WorkflowCommandStatus::Dispatched.as_str())
        .map(|(command_id, _, command)| {
            RuntimeJobCancellation::new(
                command_id,
                command.runtime_activity_key(),
                "Workflow runtime operator recovery superseded this command.",
            )
        })
        .collect::<Vec<_>>();
    let superseded_runtime_job_count =
        cancel_unfinished_runtime_jobs_for_commands_tx(tx, &cancellations).await? as u64;

    for (command_id, status, _) in &commands {
        let next_status = if status == WorkflowCommandStatus::Dispatched.as_str() {
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

    Ok((commands.len() as u64, superseded_runtime_job_count))
}

fn persist_operator_recovery_data(
    instance: &mut WorkflowInstance,
    action: WorkflowRuntimeRecoveryAction,
    reason: &str,
    actor: &str,
    previous_state: &str,
    state: &str,
    event_id: &str,
) -> anyhow::Result<()> {
    // A successful recovery ends the stop episode. Stop classification and
    // auto-recovery state must not leak into later terminal history or keep
    // recovered transcript dependency families pinned.
    let reset_feedback_repair = action == WorkflowRuntimeRecoveryAction::Unblock
        && previous_state == "blocked"
        && actor == "operator"
        && instance.data.get("feedback_repair_round").is_some();
    let mut writes = vec![
        crate::runtime::WorkflowDataWrite::remove(
            "auto_recovery",
            crate::runtime::DataProvenance::Server,
        ),
        crate::runtime::WorkflowDataWrite::remove(
            "last_stop",
            crate::runtime::DataProvenance::Server,
        ),
        crate::runtime::WorkflowDataWrite::remove(
            "stop_reason_code",
            crate::runtime::DataProvenance::Server,
        ),
        crate::runtime::WorkflowDataWrite::remove(
            "reason_class",
            crate::runtime::DataProvenance::Server,
        ),
        crate::runtime::WorkflowDataWrite::remove(
            "error_kind",
            crate::runtime::DataProvenance::Server,
        ),
        crate::runtime::WorkflowDataWrite::set(
            "last_operator_recovery",
            json!({
                "action": action.as_str(),
                "reason": reason,
                "actor": actor,
                "previous_state": previous_state,
                "state": state,
                "event_id": event_id,
            }),
            crate::runtime::DataProvenance::Server,
        ),
    ];
    if reset_feedback_repair {
        for field in [
            "feedback_repair_round",
            "feedback_repair_blocker_count",
            "feedback_repair_lane",
        ] {
            writes.push(crate::runtime::WorkflowDataWrite::remove(
                field,
                crate::runtime::DataProvenance::Server,
            ));
        }
    }
    instance.apply_data_writes(writes)
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
    if let RecoveryDispatchCommandSource::Replay(command)
    | RecoveryDispatchCommandSource::DeclarativeProgress(command) = &plan.command_source
    {
        let mut command = command.clone();
        command.dedupe_key = dedupe_key;
        return command;
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
