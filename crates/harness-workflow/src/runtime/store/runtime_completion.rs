use super::{
    apply_inline_command_side_effect, command_store, insert_decision_record_tx, insert_event_tx,
    select_instance_for_update_tx, upsert_instance_tx, validator_for_definition,
    WorkflowRuntimeStore,
};
use crate::runtime::model::{WorkflowDecisionRecord, WorkflowEvent, WorkflowInstance};
use crate::runtime::reducer::reduce_runtime_job_completed;
use crate::runtime::status::WorkflowCommandStatus;
use crate::runtime::validator::ValidationContext;
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
    let Some(decision) = reduce_runtime_job_completed(&instance, event)? else {
        return Ok(None);
    };

    persist_runtime_completion_decision_tx(tx, instance, source, event, decision)
        .await
        .map(Some)
}

async fn persist_runtime_completion_decision_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    mut instance: WorkflowInstance,
    source: &str,
    event: &WorkflowEvent,
    decision: crate::runtime::model::WorkflowDecision,
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
