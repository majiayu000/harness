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
