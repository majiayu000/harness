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
use crate::runtime::model::{
    WorkflowDecision, WorkflowDecisionRecord, WorkflowEvent, WorkflowInstance,
};
use crate::runtime::status::WorkflowCommandStatus;
use crate::runtime::{DecisionValidator, ValidationContext};

fn ensure_protected_instance_fields_match(
    current: &WorkflowInstance,
    final_instance: &WorkflowInstance,
) -> anyhow::Result<()> {
    let mut changed_fields = Vec::new();
    if current.definition_id != final_instance.definition_id {
        changed_fields.push("definition_id");
    }
    if current.definition_version != final_instance.definition_version {
        changed_fields.push("definition_version");
    }
    if current.subject != final_instance.subject {
        changed_fields.push("subject");
    }
    if current.parent_workflow_id != final_instance.parent_workflow_id {
        changed_fields.push("parent_workflow_id");
    }
    if current.lease != final_instance.lease {
        changed_fields.push("lease");
    }
    if current.created_at != final_instance.created_at {
        changed_fields.push("created_at");
    }
    if !changed_fields.is_empty() {
        anyhow::bail!(
            "workflow transition final instance changes protected fields: {}",
            changed_fields.join(", ")
        );
    }
    Ok(())
}

impl WorkflowRuntimeStore {
    /// Persist a decision attempt against the current instance.
    ///
    /// `Some(record)` means a decision record was committed, not necessarily
    /// that the instance transitioned. Callers must inspect `record.accepted`
    /// before running success side effects. `None` is reserved for a stale or
    /// terminal instance that produced no decision record.
    pub async fn apply_decision_transition(
        &self,
        transition: WorkflowDecisionTransition<'_>,
        validation_actor: &str,
    ) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
        self.apply_decision_transition_inner(transition, |current, decision, event| {
            validate_transition(current, decision, validation_actor, event.created_at)
        })
        .await
    }

    /// Persist a transition with an explicitly supplied validator.
    ///
    /// This is for callers that already resolved a durable definition snapshot
    /// that the global registry cannot currently resolve. The write still uses
    /// the same event/decision/command/instance atomic transition funnel.
    pub async fn apply_decision_transition_with_validator(
        &self,
        transition: WorkflowDecisionTransition<'_>,
        validator: &DecisionValidator,
        validation_context: ValidationContext,
    ) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
        self.apply_decision_transition_inner(transition, |current, decision, event| {
            let mut context = validation_context.clone();
            context.now = event.created_at;
            match validator.validate(current, decision, &context) {
                Ok(()) => TransitionValidation::Accepted,
                Err(error) => TransitionValidation::Rejected(error.to_string()),
            }
        })
        .await
    }

    async fn apply_decision_transition_inner<F>(
        &self,
        transition: WorkflowDecisionTransition<'_>,
        validate: F,
    ) -> anyhow::Result<Option<WorkflowDecisionRecord>>
    where
        F: FnOnce(&WorkflowInstance, &WorkflowDecision, &WorkflowEvent) -> TransitionValidation,
    {
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
        if final_instance.state != decision.next_state {
            anyhow::bail!(
                "workflow decision `{}` validates next state `{}` but final instance state is `{}`",
                decision.decision,
                decision.next_state,
                final_instance.state
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
        if current.version.checked_add(1) != Some(final_instance.version) {
            return Ok(None);
        }
        ensure_protected_instance_fields_match(&current, final_instance)?;

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
        let record = match validate(&current, decision, &event) {
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
        insert_decision_record_tx(&mut tx, &record).await?;

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
