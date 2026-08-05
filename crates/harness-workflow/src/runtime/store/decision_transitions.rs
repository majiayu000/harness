//! Decision-driven state transitions committed from a caller-supplied final
//! instance.
//!
//! Extracted from `instances.rs` so the validation funnel (GH-1784) has one
//! home; every write here must pass `validate_transition` first.

use super::{
    apply_inline_command_side_effect, command_store, commit_decision_instance_tx,
    insert_decision_record_once_tx, insert_event_tx, load_or_insert_initial_instance_tx,
    transition_validation::{validate_transition, TransitionValidation},
    validate_instance_for_persistence, WorkflowDecisionTransition,
    WorkflowRejectedDecisionTransition, WorkflowRuntimeStore,
};
use crate::runtime::model::{
    WorkflowDecision, WorkflowDecisionRecord, WorkflowEvent, WorkflowInstance,
};
use crate::runtime::status::WorkflowCommandStatus;
use crate::runtime::{DecisionValidator, ValidationContext};

/// The declarative definition pin carried in `workflow.data`.
///
/// Declarative resolution treats this hash as pinned identity: it decides
/// which definition — and therefore which validator and which legal
/// transitions — govern the workflow. It is established when the instance is
/// created and must never move afterwards, so it is a protected field even
/// though it lives inside `data` rather than in a column.
pub(super) fn definition_hash_pin(instance: &WorkflowInstance) -> Option<&serde_json::Value> {
    instance.data.get("definition_hash")
}

pub(super) fn ensure_protected_instance_fields_match(
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
    // Covers substitution, removal, and introducing a pin on a workflow that
    // never had one -- each of them re-points the instance at a different
    // definition than the one it was validated against.
    if definition_hash_pin(current) != definition_hash_pin(final_instance) {
        changed_fields.push("data.definition_hash");
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
            // The caller resolved this validator before the row was locked, so
            // re-verify under lock that it still governs the instance being
            // written. A validator from another definition, version, or content
            // hash authorizes transitions this workflow's own definition never
            // allowed (GH-1864).
            if let Err(error) = validator.binding().ensure_governs(current) {
                return TransitionValidation::Rejected(error.to_string());
            }
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
        let mut final_instance = transition.final_instance.clone();
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
        validate_instance_for_persistence(&final_instance)?;
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
        ensure_protected_instance_fields_match(&current, &final_instance)?;

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
                insert_decision_record_once_tx(&mut tx, &record).await?;
                tx.commit().await?;
                return Ok(Some(record));
            }
        };
        insert_decision_record_once_tx(&mut tx, &record).await?;

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
            if !command.requires_runtime_job() {
                apply_inline_command_side_effect(&mut final_instance, command)?;
            }
        }

        commit_decision_instance_tx(&mut tx, &current, &final_instance, &record, false).await?;

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
        insert_decision_record_once_tx(&mut tx, &record).await?;

        tx.commit().await?;
        Ok(Some(record))
    }
}

#[cfg(test)]
#[path = "decision_transitions_tests.rs"]
mod tests;

#[cfg(test)]
mod submission_guard_tests {
    use super::*;
    use crate::runtime::{WorkflowCommand, WorkflowSubject, WorkflowSubmissionDecisionTransition};
    use harness_core::db::resolve_database_url;
    use serde_json::json;

    fn instance(id: &str, state: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            "github_issue_pr",
            1,
            state,
            WorkflowSubject::new("pr", "1845"),
        )
        .with_id(id)
    }

    fn accepted_decision(instance: &WorkflowInstance) -> WorkflowDecision {
        WorkflowDecision::new(
            &instance.id,
            "addressing_feedback",
            "address_feedback",
            "local_review_gate",
            "feedback addressed",
        )
        .with_command(WorkflowCommand::enqueue_activity(
            "run_local_review",
            format!("{}-review", instance.id),
        ))
    }

    async fn seed_accepted_record(
        store: &WorkflowRuntimeStore,
        decision: &WorkflowDecision,
    ) -> anyhow::Result<WorkflowDecisionRecord> {
        let mut tx = store.pool.begin().await?;
        let event = insert_event_tx(
            &mut tx,
            &decision.workflow_id,
            "IssueSubmitted",
            "workflow-runtime-test",
            json!({}),
        )
        .await?;
        let record = WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id));
        insert_decision_record_once_tx(&mut tx, &record).await?;
        tx.commit().await?;
        Ok(record)
    }

    #[tokio::test]
    async fn accepted_submission_replay_rejects_unrelated_and_terminal_current_states(
    ) -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        for (suffix, state) in [("unrelated", "planning"), ("terminal", "done")] {
            let current = instance(&format!("stale-replay-{suffix}"), state);
            store
                .force_upsert_lifecycle_state_for_test(&current)
                .await?;
            let decision = accepted_decision(&current);
            let record = seed_accepted_record(&store, &decision).await?;
            let mut final_instance = current.clone();
            final_instance.state = decision.next_state.clone();
            final_instance.version = 1;

            let result = store
                .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                    workflow_id: &current.id,
                    expected_state: &current.state,
                    expected_version: current.version,
                    create_if_missing: None,
                    event_id: record.event_id.as_deref(),
                    new_event_id: None,
                    event_type: "IssueSubmitted",
                    source: "workflow-runtime-test",
                    payload: json!({}),
                    decision: &decision,
                    existing_record: Some(&record),
                    rejection_reason: None,
                    final_instance: Some(&final_instance),
                    command_status: WorkflowCommandStatus::Pending,
                    prompt_payload: None,
                })
                .await;

            let error = match result {
                Ok(_) => panic!("stale accepted replay should fail"),
                Err(error) => error,
            };
            assert!(error
                .to_string()
                .contains("stale workflow submission replay"));
            assert_eq!(store.get_instance(&current.id).await?, Some(current));
            assert_eq!(store.events_for(&decision.workflow_id).await?.len(), 1);
            assert_eq!(store.decisions_for(&decision.workflow_id).await?.len(), 1);
            assert!(store.commands_for(&decision.workflow_id).await?.is_empty());
        }
        Ok(())
    }

    #[tokio::test]
    async fn accepted_same_state_submission_replay_is_idempotent() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let initial = instance("same-state-submission-replay", "planning");
        let decision = WorkflowDecision::new(
            &initial.id,
            "planning",
            "continue_planning",
            "planning",
            "planning requires another pass",
        )
        .with_command(WorkflowCommand::enqueue_activity(
            "plan_issue",
            "same-state-submission-replay-plan",
        ));
        let mut final_instance = initial.clone();
        final_instance.version = 1;

        let Some(first) = store
            .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                workflow_id: &initial.id,
                expected_state: &initial.state,
                expected_version: initial.version,
                create_if_missing: Some(&initial),
                event_id: None,
                new_event_id: Some("same-state-submission-replay-event"),
                event_type: "IssueSubmitted",
                source: "workflow-runtime-test",
                payload: json!({}),
                decision: &decision,
                existing_record: None,
                rejection_reason: None,
                final_instance: Some(&final_instance),
                command_status: WorkflowCommandStatus::Pending,
                prompt_payload: None,
            })
            .await?
        else {
            anyhow::bail!("initial same-state transition should commit");
        };

        let Some(replay) = store
            .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                workflow_id: &initial.id,
                expected_state: &final_instance.state,
                expected_version: final_instance.version,
                create_if_missing: Some(&initial),
                event_id: first.record.event_id.as_deref(),
                new_event_id: None,
                event_type: "IssueSubmitted",
                source: "workflow-runtime-test",
                payload: json!({}),
                decision: &decision,
                existing_record: Some(&first.record),
                rejection_reason: None,
                final_instance: Some(&final_instance),
                command_status: WorkflowCommandStatus::Pending,
                prompt_payload: None,
            })
            .await?
        else {
            anyhow::bail!("same-state replay should reuse the accepted decision");
        };

        assert_eq!(replay.record.id, first.record.id);
        assert_eq!(replay.command_ids, first.command_ids);
        assert_eq!(store.get_instance(&initial.id).await?, Some(final_instance));
        assert_eq!(store.events_for(&initial.id).await?.len(), 1);
        assert_eq!(store.decisions_for(&initial.id).await?.len(), 1);
        assert_eq!(store.commands_for(&initial.id).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn rejected_new_submission_requires_failed_terminal_state() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        for final_state in ["ready_to_merge", "done"] {
            let initial = instance(
                &format!("rejected-final-{final_state}"),
                "addressing_feedback",
            );
            let decision = accepted_decision(&initial);
            let mut final_instance = initial.clone();
            final_instance.state = final_state.to_string();
            final_instance.version = 1;

            let result = store
                .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                    workflow_id: &initial.id,
                    expected_state: &initial.state,
                    expected_version: initial.version,
                    create_if_missing: Some(&initial),
                    event_id: None,
                    new_event_id: Some(&format!("rejected-final-{final_state}-event")),
                    event_type: "IssueSubmitted",
                    source: "workflow-runtime-test",
                    payload: json!({}),
                    decision: &decision,
                    existing_record: None,
                    rejection_reason: Some("rejected"),
                    final_instance: Some(&final_instance),
                    command_status: WorkflowCommandStatus::Pending,
                    prompt_payload: None,
                })
                .await;

            let error = match result {
                Ok(_) => panic!("non-failed rejected final state should fail"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("failed terminal state"));
            assert!(store.get_instance(&initial.id).await?.is_none());
            assert!(store.events_for(&initial.id).await?.is_empty());
            assert!(store.decisions_for(&initial.id).await?.is_empty());
            assert!(store.commands_for(&initial.id).await?.is_empty());
        }
        Ok(())
    }
}
