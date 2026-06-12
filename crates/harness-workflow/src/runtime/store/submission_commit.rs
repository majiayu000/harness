use super::{
    command_store, insert_decision_record_tx, insert_event_tx_with_id,
    insert_instance_if_absent_tx, select_instance_for_update_tx, upsert_instance_tx,
    WorkflowRuntimeStore,
};
use crate::runtime::{
    WorkflowCommandStatus, WorkflowDecision, WorkflowDecisionRecord, WorkflowInstance,
};
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

        let mut tx = self.pool.begin().await?;
        lock_submission_tx(&mut tx, transition.workflow_id).await?;
        let accepted = transition
            .existing_record
            .map(|record| record.accepted)
            .unwrap_or_else(|| transition.rejection_reason.is_none());
        let Some(current) = load_submission_instance_tx(&mut tx, &transition, accepted).await?
        else {
            return Ok(None);
        };
        if current.state != transition.expected_state
            || current.version != transition.expected_version
        {
            return Ok(None);
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
        insert_decision_record_tx(&mut tx, &record).await?;

        let mut command_ids = Vec::new();
        if record.accepted {
            let final_instance = transition.final_instance.ok_or_else(|| {
                anyhow::anyhow!("accepted workflow submission requires a final instance")
            })?;
            if let Some(prompt_payload) = transition.prompt_payload {
                upsert_prompt_payload_tx(&mut tx, prompt_payload.prompt_ref, prompt_payload.prompt)
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
            upsert_instance_tx(&mut tx, final_instance).await?;
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
    accepted: bool,
) -> anyhow::Result<Option<WorkflowInstance>> {
    if let Some(current) = select_instance_for_update_tx(tx, transition.workflow_id).await? {
        return Ok(Some(current));
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
    if !accepted {
        return Ok(Some(initial.clone()));
    }

    if insert_instance_if_absent_tx(tx, initial).await? {
        return Ok(Some(initial.clone()));
    }
    select_instance_for_update_tx(tx, transition.workflow_id).await
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
            "awaiting_feedback",
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
        final_instance.state = "awaiting_feedback".to_string();
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
