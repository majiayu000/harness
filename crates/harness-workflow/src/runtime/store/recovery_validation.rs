use super::{
    insert_event_tx, recovery_dispatch_decision, validator_for_instance, RecoveryDispatchPlan,
    WorkflowRuntimeRecoveryOutcome, WorkflowRuntimeRecoveryRequest,
};
use crate::runtime::model::WorkflowInstance;
use crate::runtime::validator::{ValidationContext, WorkflowDecisionRejectionKind};
use chrono::Utc;
use serde_json::json;

pub(super) async fn validate_request_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
    request: &WorkflowRuntimeRecoveryRequest<'_>,
    plan: &RecoveryDispatchPlan,
) -> anyhow::Result<Option<WorkflowRuntimeRecoveryOutcome>> {
    let preview = recovery_dispatch_decision(
        instance,
        request.action,
        request.reason,
        &instance.state,
        plan,
        "recovery-validation-preview",
        request.evidence,
    );
    let Some(validator) = validator_for_instance(instance)? else {
        anyhow::bail!(
            "workflow runtime recovery cannot validate definition {}",
            instance.definition_id
        );
    };
    let Err(error) = validator.validate(
        instance,
        &preview,
        &ValidationContext::new("workflow_runtime_operator_action", Utc::now()),
    ) else {
        return Ok(None);
    };
    if error.kind != WorkflowDecisionRejectionKind::MissingRequiredEvidence {
        return Err(error.into());
    }
    let detail = error.to_string();
    insert_event_tx(
        tx,
        &instance.id,
        "WorkflowRuntimeRecoveryRejected",
        "workflow_runtime_operator_action",
        json!({
            "action": request.action.as_str(),
            "actor": request.actor,
            "reason": request.reason,
            "reason_code": "missing_required_evidence",
            "state": instance.state,
            "target_state": request.target_state,
            "detail": detail,
        }),
    )
    .await?;
    Ok(Some(
        WorkflowRuntimeRecoveryOutcome::MissingRequiredEvidence {
            workflow: instance.clone(),
            detail,
        },
    ))
}
