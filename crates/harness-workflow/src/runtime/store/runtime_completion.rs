use super::{
    apply_inline_command_side_effect, command_store, insert_decision_record_tx, insert_event_tx,
    select_instance_for_update_tx, upsert_instance_tx, validator_for_definition,
    WorkflowRuntimeStore,
};
use crate::runtime::model::{
    ActivityResult, ActivityStatus, WorkflowDecision, WorkflowDecisionRecord, WorkflowEvent,
    WorkflowEvidence, WorkflowInstance,
};
use crate::runtime::reducer::reduce_runtime_job_completed;
use crate::runtime::status::WorkflowCommandStatus;
use crate::runtime::validator::{ValidationContext, WorkflowDecisionRejectionKind};
use serde_json::Value;

impl WorkflowRuntimeStore {
    pub async fn commit_parent_runtime_completion(
        &self,
        parent_workflow_id: &str,
        source: &str,
        payload: Value,
    ) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
        let mut tx = self.pool.begin().await?;
        let Some(instance) = select_instance_for_update_tx(&mut tx, parent_workflow_id).await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        // Keep the same lock order as workflow transitions: instance row first,
        // workflow event sequence lock second.
        let event = insert_event_tx(
            &mut tx,
            parent_workflow_id,
            "RuntimeJobCompleted",
            source,
            payload,
        )
        .await?;
        let decision =
            apply_runtime_completion_decision_for_instance_tx(&mut tx, instance, source, &event)
                .await?;
        tx.commit().await?;
        if let Some(decision) = decision.as_ref() {
            self.record_terminal_repo_memory_for_completion(&event, decision)
                .await;
        }
        Ok(decision)
    }

    #[cfg(test)]
    pub(crate) async fn commit_runtime_completion_decision_for_test(
        &self,
        workflow_id: &str,
        source: &str,
        payload: Value,
        decision: &crate::runtime::model::WorkflowDecision,
    ) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
        let mut tx = self.pool.begin().await?;
        let Some(instance) = select_instance_for_update_tx(&mut tx, workflow_id).await? else {
            tx.commit().await?;
            return Ok(None);
        };
        let event =
            insert_event_tx(&mut tx, workflow_id, "RuntimeJobCompleted", source, payload).await?;
        let record = persist_runtime_completion_decision_tx(
            &mut tx,
            instance,
            source,
            &event,
            decision.clone(),
        )
        .await?;
        tx.commit().await?;
        Ok(Some(record))
    }
}

pub(super) async fn apply_runtime_completion_decision_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    source: &str,
    event: &WorkflowEvent,
) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
    let Some(instance) = select_instance_for_update_tx(tx, workflow_id).await? else {
        return Ok(None);
    };
    apply_runtime_completion_decision_for_instance_tx(tx, instance, source, event).await
}

async fn apply_runtime_completion_decision_for_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: WorkflowInstance,
    source: &str,
    event: &WorkflowEvent,
) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
    let driverless_decision = driverless_structured_completion_decision(&instance, source, event)?;
    let Some(decision) = reduce_runtime_job_completed(&instance, event)? else {
        return Ok(None);
    };
    // Preserve the requested liveness rejection only when the reducer found no
    // authoritative domain outcome and would apply its generic invalid-output policy.
    // The separate blocked policy decision remains the committed outcome, so the
    // completed driver cannot leave the workflow in an unowned progress state.
    if is_generic_invalid_structured_fallback(&decision) {
        if let Some(driverless_decision) = driverless_decision {
            let rejected = persist_runtime_completion_decision_tx(
                tx,
                instance.clone(),
                source,
                event,
                driverless_decision,
            )
            .await?;
            if rejected.accepted {
                anyhow::bail!(
                    "driverless completion candidate unexpectedly passed progress validation"
                );
            }
            let rejection_reason = rejected
                .rejection_reason
                .as_deref()
                .unwrap_or("missing rejection reason");
            let policy_decision = decision.with_evidence(WorkflowEvidence::new(
                "rejected_workflow_decision",
                format!(
                    "Rejected decision record `{}` before applying blocked policy: {rejection_reason}",
                    rejected.id
                ),
            ));
            let policy = persist_runtime_completion_decision_tx(
                tx,
                instance,
                source,
                event,
                policy_decision,
            )
            .await?;
            if !policy.accepted {
                anyhow::bail!(
                    "blocked policy decision was rejected after driverless completion: {}",
                    policy
                        .rejection_reason
                        .as_deref()
                        .unwrap_or("missing rejection reason")
                );
            }
            return Ok(Some(policy));
        }
    }

    persist_runtime_completion_decision_tx(tx, instance, source, event, decision)
        .await
        .map(Some)
}

fn is_generic_invalid_structured_fallback(decision: &WorkflowDecision) -> bool {
    decision.decision == "block_invalid_agent_output"
        && decision
            .reason
            .contains("did not validate and no domain fallback was available")
}

fn driverless_structured_completion_decision(
    instance: &WorkflowInstance,
    source: &str,
    event: &WorkflowEvent,
) -> anyhow::Result<Option<WorkflowDecision>> {
    let Some(result) = event.event.get("activity_result") else {
        return Ok(None);
    };
    let result: ActivityResult = serde_json::from_value(result.clone())?;
    if result.status != ActivityStatus::Succeeded {
        return Ok(None);
    }
    let Some(decision) = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "workflow_decision")
        .find_map(|artifact| {
            serde_json::from_value::<WorkflowDecision>(artifact.artifact.clone()).ok()
        })
    else {
        return Ok(None);
    };
    let Some(validator) = validator_for_definition(&instance.definition_id) else {
        return Ok(None);
    };
    let rejection = validator.validate(
        instance,
        &decision,
        &ValidationContext::new(source, event.created_at),
    );
    match rejection {
        Err(error) if error.kind == WorkflowDecisionRejectionKind::ProgressDriverMissing => {
            Ok(Some(decision))
        }
        _ => Ok(None),
    }
}

async fn persist_runtime_completion_decision_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    mut instance: WorkflowInstance,
    source: &str,
    event: &WorkflowEvent,
    decision: WorkflowDecision,
) -> anyhow::Result<WorkflowDecisionRecord> {
    let record = match validator_for_definition(&instance.definition_id) {
        Some(validator) => match validator.validate(
            &instance,
            &decision,
            &ValidationContext::new(source, event.created_at),
        ) {
            Ok(()) => WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id.clone())),
            Err(error) => WorkflowDecisionRecord::rejected(
                decision,
                Some(event.id.clone()),
                error.to_string(),
            ),
        },
        None => WorkflowDecisionRecord::rejected(
            decision,
            Some(event.id.clone()),
            "unknown workflow definition for runtime completion",
        ),
    };
    insert_decision_record_tx(tx, &record).await?;

    if record.accepted {
        for followup in &record.decision.commands {
            let status = if followup.requires_runtime_job() {
                WorkflowCommandStatus::Pending
            } else {
                WorkflowCommandStatus::HandledInline
            };
            command_store::insert_tx(tx, &instance.id, Some(&record.id), followup, status).await?;
            if !followup.requires_runtime_job() {
                apply_inline_command_side_effect(&mut instance, followup)?;
            }
        }
        instance.state = record.decision.next_state.clone();
        instance.version = instance.version.saturating_add(1);
        upsert_instance_tx(tx, &instance).await?;
    }

    Ok(record)
}
