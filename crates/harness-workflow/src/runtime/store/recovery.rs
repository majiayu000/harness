use super::{
    command_store, enum_str, insert_decision_record_tx, insert_event_tx,
    select_instance_for_update_tx, to_jsonb_string, upsert_instance_tx, validator_for_definition,
    WorkflowInstance, WorkflowRuntimeStore,
};
use crate::runtime::model::{
    ActivityErrorKind, ActivityResult, RuntimeJob, RuntimeJobStatus, WorkflowCommand,
    WorkflowCommandType, WorkflowDecision, WorkflowDecisionRecord,
};
use crate::runtime::reducer::GITHUB_ISSUE_PR_DEFINITION_ID;
use crate::runtime::status::WorkflowCommandStatus;
use crate::runtime::validator::ValidationContext;
use serde_json::{json, Value};

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

    fn success_status(self) -> &'static str {
        match self {
            Self::Unblock => "unblocked",
            Self::Retry => "retried",
        }
    }

    fn event_type(self) -> &'static str {
        match self {
            Self::Unblock => "WorkflowRuntimeUnblocked",
            Self::Retry => "WorkflowRuntimeRetried",
        }
    }
}

pub struct WorkflowRuntimeRecoveryRequest<'a> {
    pub workflow_id: &'a str,
    pub action: WorkflowRuntimeRecoveryAction,
    pub reason: &'a str,
    pub actor: &'a str,
    pub next_state: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowRuntimeRecoveryOutcome {
    Recovered {
        workflow: WorkflowInstance,
        previous_state: String,
        event_id: String,
    },
    WrongState {
        workflow: WorkflowInstance,
    },
    NonRetryableFailure {
        workflow: WorkflowInstance,
        error_kind: ActivityErrorKind,
    },
    UnsupportedDefinition {
        workflow: WorkflowInstance,
    },
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

        if let Some(outcome) = recovery_rejection(&snapshot, request.action) {
            tx.commit().await?;
            return Ok(outcome);
        }

        let (superseded_command_count, superseded_runtime_job_count) =
            skip_superseded_active_commands_tx(&mut tx, &snapshot.id).await?;

        let Some(mut instance) =
            select_instance_for_update_tx(&mut tx, request.workflow_id).await?
        else {
            tx.rollback().await?;
            return Ok(WorkflowRuntimeRecoveryOutcome::NotFound);
        };

        if let Some(outcome) = recovery_rejection(&instance, request.action) {
            tx.rollback().await?;
            return Ok(outcome);
        }
        let previous_state = instance.state.clone();

        let event = insert_event_tx(
            &mut tx,
            &instance.id,
            request.action.event_type(),
            "workflow_runtime_operator_action",
            json!({
                "action": request.action.as_str(),
                "status": request.action.success_status(),
                "reason": request.reason,
                "actor": request.actor,
                "previous_state": previous_state,
                "state": request.next_state,
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
            request.next_state,
            &event.id,
        );
        let Some(validator) = validator_for_definition(&instance.definition_id) else {
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
            command_store::insert_tx(
                &mut tx,
                &instance.id,
                Some(&decision_record.id),
                command,
                WorkflowCommandStatus::Pending,
            )
            .await?;
        }

        instance.state = request.next_state.to_string();
        instance.version = instance.version.saturating_add(1);
        instance.lease = None;
        persist_operator_recovery_data(
            &mut instance,
            request.action,
            request.reason,
            request.actor,
            &previous_state,
            request.next_state,
            &event.id,
        );
        upsert_instance_tx(&mut tx, &instance).await?;
        tx.commit().await?;

        Ok(WorkflowRuntimeRecoveryOutcome::Recovered {
            workflow: instance,
            previous_state,
            event_id: event.id,
        })
    }
}

async fn select_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<Option<WorkflowInstance>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT data::text FROM workflow_instances WHERE id = $1")
            .bind(workflow_id)
            .fetch_optional(&mut **tx)
            .await?;
    row.map(|(data,)| serde_json::from_str(&data))
        .transpose()
        .map_err(Into::into)
}

fn recovery_rejection(
    instance: &WorkflowInstance,
    action: WorkflowRuntimeRecoveryAction,
) -> Option<WorkflowRuntimeRecoveryOutcome> {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID {
        return Some(WorkflowRuntimeRecoveryOutcome::UnsupportedDefinition {
            workflow: instance.clone(),
        });
    }

    if instance.state != action.expected_state() {
        return Some(WorkflowRuntimeRecoveryOutcome::WrongState {
            workflow: instance.clone(),
        });
    }

    if action == WorkflowRuntimeRecoveryAction::Retry {
        if let Some(error_kind) = stopped_error_kind(&instance.data).filter(|kind| {
            matches!(
                kind,
                ActivityErrorKind::Fatal | ActivityErrorKind::Configuration
            )
        }) {
            return Some(WorkflowRuntimeRecoveryOutcome::NonRetryableFailure {
                workflow: instance.clone(),
                error_kind,
            });
        }
    }

    None
}

async fn skip_superseded_active_commands_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<(u64, u64)> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, status, data::text
         FROM workflow_commands
         WHERE workflow_id = $1
           AND status IN ($2, $3, $4)
         FOR UPDATE",
    )
    .bind(workflow_id)
    .bind(WorkflowCommandStatus::Pending.as_str())
    .bind(WorkflowCommandStatus::Dispatching.as_str())
    .bind(WorkflowCommandStatus::Dispatched.as_str())
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
            "UPDATE workflow_commands
             SET status = $2,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1",
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
        "SELECT id, data::text
         FROM runtime_jobs
         WHERE command_id = $1
           AND status IN ($2, $3)
         FOR UPDATE",
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
            "UPDATE runtime_jobs
             SET status = $1,
                 not_before = $2,
                 data = $3::jsonb,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $4",
        )
        .bind(&status)
        .bind(job.not_before)
        .bind(&updated)
        .bind(job_id)
        .execute(&mut **tx)
        .await?;
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
    next_state: &str,
    event_id: &str,
) -> WorkflowDecision {
    WorkflowDecision::new(
        &instance.id,
        previous_state,
        format!("operator_runtime_{}", action.as_str()),
        next_state,
        format!(
            "operator requested workflow runtime {} after resolving the stopped condition",
            action.as_str()
        ),
    )
    .with_command(recovery_dispatch_command(
        instance, action, reason, event_id,
    ))
}

fn recovery_dispatch_command(
    instance: &WorkflowInstance,
    action: WorkflowRuntimeRecoveryAction,
    reason: &str,
    event_id: &str,
) -> WorkflowCommand {
    let remote_fact_hash = optional_string_field(&instance.data, "last_remote_fact_hash");
    let dispatch_fact_hash = remote_fact_hash.clone();
    WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        format!(
            "operator-recovery:{}:{}:{}",
            action.as_str(),
            instance.id,
            event_id
        ),
        json!({
            "activity": "implement_issue",
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
        }),
    )
}

fn stopped_error_kind(data: &Value) -> Option<ActivityErrorKind> {
    data.get("error_kind")
        .cloned()
        .or_else(|| data.pointer("/last_stop/error_kind").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
}

fn optional_string_field(data: &Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}
