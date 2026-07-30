use super::{
    command_store,
    decision_transitions::ensure_protected_instance_fields_match,
    insert_decision_record_tx, insert_event_tx_with_id, insert_instance_if_absent_tx,
    select_instance_for_update_tx,
    transition_validation::{validate_transition_with_context, TransitionValidation},
    upsert_instance_tx, WorkflowRuntimeStore,
};
use crate::runtime::{
    ValidationContext, WorkflowCommandStatus, WorkflowDecision, WorkflowDecisionRecord,
    WorkflowInstance, WorkflowTerminalState,
};
use chrono::Utc;
use serde_json::Value;

pub struct WorkflowSubmissionDecisionTransition<'a> {
    pub workflow_id: &'a str,
    pub expected_state: &'a str,
    pub expected_version: u64,
    pub create_if_missing: Option<&'a WorkflowInstance>,
    pub event_id: Option<&'a str>,
    pub new_event_id: Option<&'a str>,
    pub event_type: &'a str,
    pub source: &'a str,
    pub payload: Value,
    pub decision: &'a WorkflowDecision,
    pub existing_record: Option<&'a WorkflowDecisionRecord>,
    pub rejection_reason: Option<&'a str>,
    pub final_instance: Option<&'a WorkflowInstance>,
    pub command_status: WorkflowCommandStatus,
    pub prompt_payload: Option<WorkflowSubmissionPromptPayload<'a>>,
}

pub struct WorkflowSubmissionPromptPayload<'a> {
    pub prompt_ref: &'a str,
    pub prompt: &'a str,
    pub previous_prompt_ref: Option<&'a str>,
}

pub struct WorkflowSubmissionDecisionCommit {
    pub record: WorkflowDecisionRecord,
    pub command_ids: Vec<String>,
}

impl WorkflowRuntimeStore {
    pub async fn commit_submission_decision_transition(
        &self,
        transition: WorkflowSubmissionDecisionTransition<'_>,
    ) -> anyhow::Result<Option<WorkflowSubmissionDecisionCommit>> {
        let decision = transition
            .existing_record
            .map(|record| &record.decision)
            .unwrap_or(transition.decision);
        if decision.workflow_id != transition.workflow_id {
            anyhow::bail!(
                "workflow submission decision `{}` targets `{}` but transition targets `{}`",
                decision.decision,
                decision.workflow_id,
                transition.workflow_id
            );
        }
        if let Some(record) = transition.existing_record {
            if record.workflow_id != transition.workflow_id {
                anyhow::bail!(
                    "workflow submission record `{}` targets `{}` but transition targets `{}`",
                    record.id,
                    record.workflow_id,
                    transition.workflow_id
                );
            }
        }
        if let Some(final_instance) = transition.final_instance {
            if final_instance.id != transition.workflow_id {
                anyhow::bail!(
                    "workflow submission final instance `{}` does not match transition `{}`",
                    final_instance.id,
                    transition.workflow_id
                );
            }
        }
        if transition.rejection_reason.is_some()
            && transition.create_if_missing.is_some()
            && transition.final_instance.is_none()
        {
            anyhow::bail!(
                "rejected new workflow submission `{}` requires a terminal final instance",
                transition.workflow_id
            );
        }

        let mut tx = self.pool.begin().await?;
        lock_submission_tx(&mut tx, transition.workflow_id).await?;
        let Some((current, created_for_submission)) =
            load_submission_instance_tx(&mut tx, &transition).await?
        else {
            return Ok(None);
        };
        if current.state != transition.expected_state
            || current.version != transition.expected_version
        {
            return Ok(None);
        }
        let replays_applied_instance = match transition
            .existing_record
            .filter(|record| record.accepted)
        {
            Some(record) if current.state == record.decision.observed_state => false,
            Some(record) if current.state == record.decision.next_state => true,
            Some(record) => {
                anyhow::bail!(
                    "stale workflow submission replay `{}` observed current state `{}` outside decision transition `{} -> {}`",
                    record.id,
                    current.state,
                    record.decision.observed_state,
                    record.decision.next_state
                );
            }
            None => false,
        };
        if let Some(final_instance) = transition.final_instance {
            ensure_protected_instance_fields_match(&current, final_instance)?;
            let rejected_final = transition.rejection_reason.is_some()
                || transition
                    .existing_record
                    .is_some_and(|record| !record.accepted);
            if rejected_final {
                if !created_for_submission
                    || transition.existing_record.is_some()
                    || transition.rejection_reason.is_none()
                {
                    anyhow::bail!(
                        "rejected workflow submission final instance is only allowed for a newly created submission"
                    );
                }
                if final_instance.terminal_state() != Some(WorkflowTerminalState::Failed) {
                    anyhow::bail!(
                        "rejected workflow submission final instance must use the definition-specific failed terminal state"
                    );
                }
            }
            let expected_final_version = if replays_applied_instance {
                current.version
            } else {
                current.version.checked_add(1).ok_or_else(|| {
                    anyhow::anyhow!(
                        "workflow submission `{}` current version cannot advance",
                        transition.workflow_id
                    )
                })?
            };
            if final_instance.version != expected_final_version {
                anyhow::bail!(
                    "workflow submission final instance has version `{}` but expected `{expected_final_version}`",
                    final_instance.version
                );
            }
        }

        let event_id = if let Some(event_id) = transition.event_id {
            event_id.to_string()
        } else {
            insert_event_tx_with_id(
                &mut tx,
                transition.workflow_id,
                transition.event_type,
                transition.source,
                transition.payload,
                transition.new_event_id,
            )
            .await?
            .id
        };
        let record = match transition.existing_record {
            Some(record) => {
                if record.event_id.as_deref() != Some(event_id.as_str()) {
                    anyhow::bail!(
                        "workflow submission record `{}` is linked to event `{:?}` but transition uses `{}`",
                        record.id,
                        record.event_id,
                        event_id
                    );
                }
                record.clone()
            }
            None => match transition.rejection_reason {
                Some(reason) => WorkflowDecisionRecord::rejected(
                    transition.decision.clone(),
                    Some(event_id),
                    reason,
                ),
                None => {
                    WorkflowDecisionRecord::accepted(transition.decision.clone(), Some(event_id))
                }
            },
        };
        let validate_as_new_transition = record.accepted
            && transition
                .existing_record
                .is_none_or(|_| current.state == record.decision.observed_state);
        if validate_as_new_transition {
            let validation_context = if current.is_terminal() {
                ValidationContext::new("workflow-policy", Utc::now()).allow_terminal_reopen()
            } else {
                ValidationContext::new("workflow-policy", Utc::now())
            };
            match validate_transition_with_context(&current, &record.decision, &validation_context)
            {
                TransitionValidation::Accepted => {}
                TransitionValidation::Rejected(reason) if transition.existing_record.is_none() => {
                    let rejected = WorkflowDecisionRecord::rejected(
                        record.decision.clone(),
                        record.event_id.clone(),
                        reason,
                    );
                    insert_decision_record_tx(&mut tx, &rejected).await?;
                    tx.commit().await?;
                    return Ok(Some(WorkflowSubmissionDecisionCommit {
                        record: rejected,
                        command_ids: Vec::new(),
                    }));
                }
                TransitionValidation::Rejected(reason) => {
                    anyhow::bail!(
                        "existing workflow submission decision failed validation: {reason}"
                    );
                }
            }
        }
        insert_decision_record_tx(&mut tx, &record).await?;

        let mut command_ids = Vec::new();
        if let Some(final_instance) = transition.final_instance {
            if record.accepted {
                if final_instance.state != record.decision.next_state {
                    anyhow::bail!(
                        "workflow submission decision `{}` validates next state `{}` but final instance state is `{}`",
                        record.decision.decision,
                        record.decision.next_state,
                        final_instance.state
                    );
                }
                if let Some(prompt_payload) = transition.prompt_payload {
                    upsert_prompt_payload_tx(
                        &mut tx,
                        prompt_payload.prompt_ref,
                        prompt_payload.prompt,
                    )
                    .await?;
                    if let Some(previous_prompt_ref) = prompt_payload.previous_prompt_ref {
                        delete_prompt_payload_tx(&mut tx, previous_prompt_ref).await?;
                    }
                }
                for command in &record.decision.commands {
                    command_ids.push(
                        command_store::insert_tx(
                            &mut tx,
                            transition.workflow_id,
                            Some(&record.id),
                            command,
                            transition.command_status,
                        )
                        .await?,
                    );
                }
            }
            upsert_instance_tx(&mut tx, final_instance).await?;
        } else if record.accepted {
            anyhow::bail!("accepted workflow submission requires a final instance");
        }

        tx.commit().await?;
        Ok(Some(WorkflowSubmissionDecisionCommit {
            record,
            command_ids,
        }))
    }
}

async fn lock_submission_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("workflow_submission:{workflow_id}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn load_submission_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    transition: &WorkflowSubmissionDecisionTransition<'_>,
) -> anyhow::Result<Option<(WorkflowInstance, bool)>> {
    if let Some(current) = select_instance_for_update_tx(tx, transition.workflow_id).await? {
        return Ok(Some((current, false)));
    }

    let Some(initial) = transition.create_if_missing else {
        return Ok(None);
    };
    if initial.id != transition.workflow_id {
        anyhow::bail!(
            "initial workflow instance `{}` does not match workflow `{}`",
            initial.id,
            transition.workflow_id
        );
    }
    if initial.state != transition.expected_state || initial.version != transition.expected_version
    {
        return Ok(None);
    }
    if insert_instance_if_absent_tx(tx, initial).await? {
        return Ok(Some((initial.clone(), true)));
    }
    Ok(select_instance_for_update_tx(tx, transition.workflow_id)
        .await?
        .map(|instance| (instance, false)))
}

async fn upsert_prompt_payload_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    prompt_ref: &str,
    prompt: &str,
) -> anyhow::Result<()> {
    if prompt_ref.trim().is_empty() {
        anyhow::bail!("workflow prompt payload prompt_ref must not be empty");
    }
    sqlx::query(
        "INSERT INTO workflow_prompt_payloads (prompt_ref, prompt)
         VALUES ($1, $2)
         ON CONFLICT (prompt_ref) DO UPDATE SET
            prompt = EXCLUDED.prompt,
            updated_at = CURRENT_TIMESTAMP",
    )
    .bind(prompt_ref)
    .bind(prompt)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn delete_prompt_payload_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    prompt_ref: &str,
) -> anyhow::Result<()> {
    if prompt_ref.trim().is_empty() {
        return Ok(());
    }
    sqlx::query("DELETE FROM workflow_prompt_payloads WHERE prompt_ref = $1")
        .bind(prompt_ref)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{WorkflowCommand, WorkflowCommandRecord, WorkflowSubject};
    use harness_core::db::resolve_database_url;
    use serde_json::json;

    fn submission_test_instance(id: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            "github_issue_pr",
            1,
            "addressing_feedback",
            WorkflowSubject::new("pr", "77"),
        )
        .with_id(id)
        .with_data(json!({"project_id": "/project-a", "pr_number": 77}))
    }

    fn submission_test_decision(instance: &WorkflowInstance) -> WorkflowDecision {
        WorkflowDecision::new(
            &instance.id,
            "addressing_feedback",
            "address_feedback",
            "local_review_gate",
            "review feedback was addressed",
        )
        .with_command(WorkflowCommand::enqueue_activity(
            "run_local_review",
            format!("{}-local-review", instance.id),
        ))
    }

    async fn invalid_submission_final_error(
        store: &WorkflowRuntimeStore,
        current: &WorkflowInstance,
        decision: &WorkflowDecision,
        create_if_missing: Option<&WorkflowInstance>,
        existing_record: Option<&WorkflowDecisionRecord>,
        rejection_reason: Option<&str>,
        final_instance: &WorkflowInstance,
    ) -> anyhow::Error {
        match store
            .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                workflow_id: &current.id,
                expected_state: &current.state,
                expected_version: current.version,
                create_if_missing,
                event_id: existing_record.and_then(|record| record.event_id.as_deref()),
                new_event_id: existing_record
                    .is_none()
                    .then_some("invalid-submission-final-event"),
                event_type: "IssueSubmitted",
                source: "workflow-runtime-test",
                payload: json!({}),
                decision,
                existing_record,
                rejection_reason,
                final_instance: Some(final_instance),
                command_status: WorkflowCommandStatus::Pending,
                prompt_payload: None,
            })
            .await
        {
            Ok(_) => panic!("invalid final instance should fail"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn accepted_submission_rejects_identity_substitution_and_invalid_version(
    ) -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
        let initial = submission_test_instance("submission-accepted-identity-version");
        store.upsert_instance(&initial).await?;
        let decision = submission_test_decision(&initial);
        let mut final_instance = initial.clone();
        final_instance.state = decision.next_state.clone();
        final_instance.version = 1;

        let mut substituted = final_instance.clone();
        substituted.definition_version = 2;
        let error = invalid_submission_final_error(
            &store,
            &initial,
            &decision,
            None,
            None,
            None,
            &substituted,
        )
        .await;
        assert!(error.to_string().contains("definition_version"));

        let mut invalid_version = final_instance;
        invalid_version.version = 0;
        let error = invalid_submission_final_error(
            &store,
            &initial,
            &decision,
            None,
            None,
            None,
            &invalid_version,
        )
        .await;
        assert!(error.to_string().contains("expected `1`"));
        assert_eq!(store.get_instance(&initial.id).await?, Some(initial));
        Ok(())
    }

    #[tokio::test]
    async fn rejected_submission_rejects_identity_substitution_and_invalid_version(
    ) -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
        let initial = submission_test_instance("submission-rejected-identity-version");
        let decision = submission_test_decision(&initial);
        let mut final_instance = initial.clone();
        final_instance.state = "failed".to_string();
        final_instance.version = 1;

        let mut substituted = final_instance.clone();
        substituted.subject = WorkflowSubject::new("issue", "substituted");
        let error = invalid_submission_final_error(
            &store,
            &initial,
            &decision,
            Some(&initial),
            None,
            Some("rejected"),
            &substituted,
        )
        .await;
        assert!(error.to_string().contains("subject"));

        let mut invalid_version = final_instance;
        invalid_version.version = 7;
        let error = invalid_submission_final_error(
            &store,
            &initial,
            &decision,
            Some(&initial),
            None,
            Some("rejected"),
            &invalid_version,
        )
        .await;
        assert!(error.to_string().contains("expected `1`"));
        assert!(store.get_instance(&initial.id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn submission_replay_rejects_identity_substitution_and_version_advance(
    ) -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
        let initial = submission_test_instance("submission-replay-identity-version");
        let decision = submission_test_decision(&initial);
        let mut final_instance = initial.clone();
        final_instance.state = decision.next_state.clone();
        final_instance.version = 1;
        let first_commit = store
            .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                workflow_id: &initial.id,
                expected_state: &initial.state,
                expected_version: initial.version,
                create_if_missing: Some(&initial),
                event_id: None,
                new_event_id: Some("submission-replay-identity-version-event"),
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
            .expect("initial transition should commit");

        let mut substituted = final_instance.clone();
        substituted.parent_workflow_id = Some("substituted-parent".to_string());
        let error = invalid_submission_final_error(
            &store,
            &final_instance,
            &decision,
            None,
            Some(&first_commit.record),
            None,
            &substituted,
        )
        .await;
        assert!(error.to_string().contains("parent_workflow_id"));

        let mut advanced_version = final_instance.clone();
        advanced_version.version = 2;
        let error = invalid_submission_final_error(
            &store,
            &final_instance,
            &decision,
            None,
            Some(&first_commit.record),
            None,
            &advanced_version,
        )
        .await;
        assert!(error.to_string().contains("expected `1`"));
        assert_eq!(store.get_instance(&initial.id).await?, Some(final_instance));
        Ok(())
    }

    #[tokio::test]
    async fn submission_replay_keeps_completed_commands_when_repairing_pending_commands(
    ) -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
        let initial = WorkflowInstance::new(
            "github_issue_pr",
            1,
            "addressing_feedback",
            WorkflowSubject::new("pr", "77"),
        )
        .with_id("submission-replay-keeps-completed-commands")
        .with_data(json!({
            "project_id": "/project-a",
            "pr_number": 77,
        }));
        let decision = WorkflowDecision::new(
            &initial.id,
            "addressing_feedback",
            "address_feedback",
            "local_review_gate",
            "review feedback was addressed",
        )
        .with_command(WorkflowCommand::enqueue_activity(
            "run_local_review",
            "submission-local-review",
        ))
        .with_command(WorkflowCommand::enqueue_activity(
            "inspect_pr_feedback",
            "submission-remote-feedback",
        ));
        let mut final_instance = initial.clone();
        final_instance.state = "local_review_gate".to_string();
        final_instance.version = final_instance.version.saturating_add(1);
        final_instance.data = json!({
            "project_id": "/project-a",
            "pr_number": 77,
            "last_decision": "address_feedback",
        });

        let first_commit = store
            .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                workflow_id: &initial.id,
                expected_state: &initial.state,
                expected_version: initial.version,
                create_if_missing: Some(&initial),
                event_id: None,
                new_event_id: Some("submission-replay-event-1"),
                event_type: "IssueSubmitted",
                source: "workflow-runtime-test",
                payload: json!({"task_id": "feedback-submission"}),
                decision: &decision,
                existing_record: None,
                rejection_reason: None,
                final_instance: Some(&final_instance),
                command_status: WorkflowCommandStatus::Pending,
                prompt_payload: None,
            })
            .await?
            .expect("initial submission commit should be accepted");
        let commands = store.commands_for(&initial.id).await?;
        assert_eq!(commands.len(), 2);
        let completed_command_id = command_by_dedupe(&commands, "submission-local-review")
            .id
            .clone();
        let pending_command_id = command_by_dedupe(&commands, "submission-remote-feedback")
            .id
            .clone();
        store
            .mark_command_status(&completed_command_id, WorkflowCommandStatus::Completed)
            .await?;

        let replay_commit = store
            .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                workflow_id: &initial.id,
                expected_state: &final_instance.state,
                expected_version: final_instance.version,
                create_if_missing: Some(&initial),
                event_id: first_commit.record.event_id.as_deref(),
                new_event_id: None,
                event_type: "IssueSubmitted",
                source: "workflow-runtime-test",
                payload: json!({"task_id": "feedback-submission"}),
                decision: &decision,
                existing_record: Some(&first_commit.record),
                rejection_reason: None,
                final_instance: Some(&final_instance),
                command_status: WorkflowCommandStatus::Pending,
                prompt_payload: None,
            })
            .await?
            .expect("submission replay should reuse the accepted decision");

        assert_eq!(replay_commit.command_ids, first_commit.command_ids);
        let commands = store.commands_for(&initial.id).await?;
        assert_eq!(commands.len(), 2);
        assert_eq!(
            command_by_dedupe(&commands, "submission-local-review").status,
            WorkflowCommandStatus::Completed
        );
        assert_eq!(
            store
                .get_command(&pending_command_id)
                .await?
                .expect("pending command should remain present")
                .status,
            WorkflowCommandStatus::Pending
        );
        Ok(())
    }

    #[tokio::test]
    async fn submission_commit_rejects_allowlist_violating_transition() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
        let initial = WorkflowInstance::new(
            "github_issue_pr",
            1,
            "discovered",
            WorkflowSubject::new("issue", "issue:1784"),
        )
        .with_id("submission-validator-bypass")
        .with_data(json!({"project_id": "/project-a", "issue_number": 1784}));
        let decision = WorkflowDecision::new(
            &initial.id,
            "discovered",
            "skip_to_merge_gate",
            "ready_to_merge",
            "invalid direct promotion",
        );
        let mut final_instance = initial.clone();
        final_instance.state = "ready_to_merge".to_string();
        final_instance.version = 1;

        let commit = store
            .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                workflow_id: &initial.id,
                expected_state: &initial.state,
                expected_version: initial.version,
                create_if_missing: Some(&initial),
                event_id: None,
                new_event_id: Some("submission-validator-bypass-event"),
                event_type: "IssueSubmitted",
                source: "workflow-runtime-test",
                payload: json!({"task_id": "invalid-submission"}),
                decision: &decision,
                existing_record: None,
                rejection_reason: None,
                final_instance: Some(&final_instance),
                command_status: WorkflowCommandStatus::Pending,
                prompt_payload: None,
            })
            .await?
            .expect("submission decision should be recorded");

        assert!(!commit.record.accepted);
        assert!(commit.command_ids.is_empty());
        assert_eq!(
            store
                .get_instance(&initial.id)
                .await?
                .expect("workflow")
                .state,
            "discovered"
        );
        Ok(())
    }

    fn command_by_dedupe<'a>(
        commands: &'a [WorkflowCommandRecord],
        dedupe_key: &str,
    ) -> &'a WorkflowCommandRecord {
        commands
            .iter()
            .find(|command| command.command.dedupe_key == dedupe_key)
            .expect("command should be present")
    }
}
