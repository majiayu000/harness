//! Decision-driven state transitions committed from a caller-supplied final
//! instance.
//!
//! Extracted from `instances.rs` so the validation funnel (GH-1784) has one
//! home; every write here must pass `validate_transition` first.

use super::{
    command_store, insert_decision_record_tx, insert_event_tx, load_or_insert_initial_instance_tx,
    to_jsonb_string,
    transition_validation::{validate_transition, TransitionValidation},
    WorkflowDecisionTransition, WorkflowRejectedDecisionTransition, WorkflowRuntimeStore,
};
use crate::runtime::model::WorkflowDecisionRecord;
#[cfg(test)]
use crate::runtime::model::{WorkflowDecision, WorkflowInstance};
use crate::runtime::status::WorkflowCommandStatus;

impl WorkflowRuntimeStore {
    pub async fn apply_decision_transition(
        &self,
        transition: WorkflowDecisionTransition<'_>,
    ) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
        let final_instance = transition.final_instance;
        let decision = transition.decision;
        if decision.workflow_id != final_instance.id {
            anyhow::bail!(
                "workflow decision `{}` targets `{}` but final instance is `{}`",
                decision.decision,
                decision.workflow_id,
                final_instance.id
            );
        }
        let mut tx = self.pool.begin().await?;
        let Some(current) = load_or_insert_initial_instance_tx(
            &mut tx,
            &final_instance.id,
            transition.expected_state,
            transition.create_if_missing,
        )
        .await?
        else {
            return Ok(None);
        };
        if current.is_terminal() || current.state != transition.expected_state {
            return Ok(None);
        }

        let event = insert_event_tx(
            &mut tx,
            &final_instance.id,
            transition.event_type,
            transition.source,
            transition.payload,
        )
        .await?;

        // GH-1784: this path used to write `final_instance` verbatim after only
        // an expected-state and non-terminality check, so the transition
        // allowlist and required commands/evidence were never consulted.
        let record =
            match validate_transition(&current, decision, transition.source, event.created_at) {
                TransitionValidation::Accepted => {
                    WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id))
                }
                TransitionValidation::Rejected(reason) => {
                    let record =
                        WorkflowDecisionRecord::rejected(decision.clone(), Some(event.id), reason);
                    insert_decision_record_tx(&mut tx, &record).await?;
                    tx.commit().await?;
                    return Ok(Some(record));
                }
            };
        let decision_data = to_jsonb_string(&record)?;
        sqlx::query(
            "INSERT INTO workflow_decisions
                (id, workflow_id, event_id, accepted, data, rejection_reason)
             VALUES ($1, $2, $3, $4, $5::jsonb, $6)
             ON CONFLICT (id) DO UPDATE SET
                accepted = EXCLUDED.accepted,
                data = EXCLUDED.data,
                rejection_reason = EXCLUDED.rejection_reason",
        )
        .bind(&record.id)
        .bind(&record.workflow_id)
        .bind(&record.event_id)
        .bind(record.accepted)
        .bind(&decision_data)
        .bind(&record.rejection_reason)
        .execute(&mut *tx)
        .await?;

        for command in &decision.commands {
            let status = if command.requires_runtime_job() {
                transition.command_status
            } else {
                WorkflowCommandStatus::HandledInline
            };
            command_store::insert_tx(
                &mut tx,
                &final_instance.id,
                Some(&record.id),
                command,
                status,
            )
            .await?;
        }

        let instance_data = to_jsonb_string(final_instance)?;
        sqlx::query(
            "INSERT INTO workflow_instances
                (id, definition_id, state, subject_type, subject_key, parent_workflow_id, data, version)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8)
             ON CONFLICT (id) DO UPDATE SET
                definition_id = EXCLUDED.definition_id,
                state = EXCLUDED.state,
                subject_type = EXCLUDED.subject_type,
                subject_key = EXCLUDED.subject_key,
                parent_workflow_id = EXCLUDED.parent_workflow_id,
                data = EXCLUDED.data,
                version = EXCLUDED.version,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&final_instance.id)
        .bind(&final_instance.definition_id)
        .bind(&final_instance.state)
        .bind(&final_instance.subject.subject_type)
        .bind(&final_instance.subject.subject_key)
        .bind(&final_instance.parent_workflow_id)
        .bind(&instance_data)
        .bind(final_instance.version as i64)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Some(record))
    }

    pub async fn record_rejected_decision_transition(
        &self,
        transition: WorkflowRejectedDecisionTransition<'_>,
    ) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
        let decision = transition.decision;
        let mut tx = self.pool.begin().await?;
        let Some(current) = load_or_insert_initial_instance_tx(
            &mut tx,
            &decision.workflow_id,
            transition.expected_state,
            transition.create_if_missing,
        )
        .await?
        else {
            return Ok(None);
        };
        if current.is_terminal() || current.state != transition.expected_state {
            return Ok(None);
        }

        let event = insert_event_tx(
            &mut tx,
            &decision.workflow_id,
            transition.event_type,
            transition.source,
            transition.payload,
        )
        .await?;
        let record =
            WorkflowDecisionRecord::rejected(decision.clone(), Some(event.id), transition.reason);
        insert_decision_record_tx(&mut tx, &record).await?;

        tx.commit().await?;
        Ok(Some(record))
    }
}

#[cfg(test)]
#[path = "decision_transitions_tests.rs"]
mod tests;
