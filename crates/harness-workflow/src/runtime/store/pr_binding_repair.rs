use super::{
    commit_same_state_instance_tx, insert_event_tx_with_id, select_instance_for_update_tx,
    WorkflowRuntimeStore,
};
use crate::runtime::{
    DataProvenance, WorkflowCommand, WorkflowCommandStatus, WorkflowCommandType, WorkflowDataWrite,
    WorkflowDecisionRecord, WorkflowInstance,
};
use anyhow::Context;
use serde_json::json;

#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowPrBindingRepairOutcome {
    Repaired {
        instance: Box<WorkflowInstance>,
        command_id: String,
        pr_number: u64,
        pr_url: String,
    },
    NoBindPrCommand,
    StaleInstance,
}

impl WorkflowRuntimeStore {
    /// Repair a missing `pr_number`/`pr_url` binding from command evidence.
    ///
    /// The evidence must be a live (non-superseded) `BindPr` command minted by
    /// an accepted decision of the same workflow — the decision row is
    /// append-only (GH-1865), so proving the decision carries this command's
    /// dedupe key ties the repair to what was actually authorized (GH-1864).
    /// A standalone command, one whose decision was rejected, or a superseded
    /// attempt proves nothing and leaves the binding untouched.
    ///
    /// A successful repair records a durable `PrBindingRepaired` event in the
    /// same transaction so the write carries its own provenance.
    pub async fn repair_pr_binding_from_latest_command(
        &self,
        workflow_id: &str,
        expected_version: u64,
        source: &str,
    ) -> anyhow::Result<WorkflowPrBindingRepairOutcome> {
        let mut tx = self.pool.begin().await?;
        let Some(current) = select_instance_for_update_tx(&mut tx, workflow_id).await? else {
            tx.rollback().await?;
            return Ok(WorkflowPrBindingRepairOutcome::StaleInstance);
        };
        if current.version != expected_version {
            tx.rollback().await?;
            return Ok(WorkflowPrBindingRepairOutcome::StaleInstance);
        }

        let rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
            "SELECT id, decision_id, data::text
             FROM workflow_commands
             WHERE workflow_id = $1
               AND command_type = $2
               AND status <> $3
             ORDER BY created_at DESC, id DESC
             FOR UPDATE",
        )
        .bind(workflow_id)
        .bind(WorkflowCommandType::BindPr.as_str())
        .bind(WorkflowCommandStatus::Superseded.as_str())
        .fetch_all(&mut *tx)
        .await?;
        let mut evidence: Option<(String, WorkflowCommand, String)> = None;
        for (command_id, decision_id, data) in rows {
            let Some(decision_id) = decision_id else {
                continue;
            };
            let decision_row: Option<(String,)> = sqlx::query_as(
                "SELECT data::text FROM workflow_decisions
                 WHERE id = $1 AND workflow_id = $2 AND accepted",
            )
            .bind(&decision_id)
            .bind(workflow_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((decision_data,)) = decision_row else {
                continue;
            };
            let record: WorkflowDecisionRecord = serde_json::from_str(&decision_data)?;
            let command: WorkflowCommand = serde_json::from_str(&data)?;
            let decision_carries_command = record.decision.commands.iter().any(|carried| {
                carried.command_type == WorkflowCommandType::BindPr
                    && carried.dedupe_key == command.dedupe_key
            });
            if decision_carries_command {
                evidence = Some((command_id, command, decision_id));
                break;
            }
        }
        let Some((command_id, command, decision_id)) = evidence else {
            tx.rollback().await?;
            return Ok(WorkflowPrBindingRepairOutcome::NoBindPrCommand);
        };
        let pr_number = command
            .command
            .get("pr_number")
            .and_then(serde_json::Value::as_u64)
            .context("bind_pr command is missing pr_number")?;
        let pr_url = command
            .command
            .get("pr_url")
            .and_then(serde_json::Value::as_str)
            .context("bind_pr command is missing pr_url")?
            .to_string();

        let mut target = current.clone();
        if !target.data.is_object() {
            target.replace_classified_data(json!({}), DataProvenance::Server);
        }
        // The repaired binding is replayed from a bind_pr command the agent
        // produced, so it stays agent-classified rather than being laundered
        // into server data by passing through a server-side repair path.
        target.apply_data_writes([
            WorkflowDataWrite::set("pr_number", json!(pr_number), DataProvenance::Agent),
            WorkflowDataWrite::set("pr_url", json!(pr_url.clone()), DataProvenance::Agent),
        ])?;
        target.version = target.version.saturating_add(1);
        commit_same_state_instance_tx(&mut tx, &current, &target).await?;
        insert_event_tx_with_id(
            &mut tx,
            workflow_id,
            "PrBindingRepaired",
            source,
            json!({
                "command_id": command_id,
                "decision_id": decision_id,
                "pr_number": pr_number,
                "pr_url": pr_url,
                "previous_pr_number": current.data.get("pr_number").cloned(),
                "previous_pr_url": current.data.get("pr_url").cloned(),
            }),
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(WorkflowPrBindingRepairOutcome::Repaired {
            instance: Box::new(target),
            command_id,
            pr_number,
            pr_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{WorkflowDecision, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID};
    use harness_core::db::resolve_database_url;

    fn workflow(id: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "awaiting_feedback",
            WorkflowSubject::new("issue", "issue:1784"),
        )
        .with_id(id)
    }

    fn bind_pr(dedupe_key: &str) -> WorkflowCommand {
        WorkflowCommand::bind_pr(
            1845,
            "https://github.com/majiayu000/harness/pull/1845",
            dedupe_key,
        )
    }

    fn bind_pr_decision(workflow_id: &str, command: &WorkflowCommand) -> WorkflowDecision {
        WorkflowDecision::new(
            workflow_id,
            "awaiting_feedback",
            "bind_pr",
            "awaiting_feedback",
            "agent reported the pull request",
        )
        .with_command(command.clone())
    }

    async fn repair_events(
        store: &WorkflowRuntimeStore,
        workflow_id: &str,
    ) -> anyhow::Result<Vec<crate::runtime::WorkflowEvent>> {
        Ok(store
            .events_for(workflow_id)
            .await?
            .into_iter()
            .filter(|event| event.event_type == "PrBindingRepaired")
            .collect())
    }

    #[tokio::test]
    async fn pr_binding_repair_requires_bound_decision_evidence() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let instance = workflow("pr-binding-repair");
        store
            .force_upsert_lifecycle_state_for_test(&instance)
            .await?;

        assert_eq!(
            store
                .repair_pr_binding_from_latest_command(&instance.id, 0, "test")
                .await?,
            WorkflowPrBindingRepairOutcome::NoBindPrCommand
        );

        // A standalone command with no decision behind it is not evidence.
        let standalone = bind_pr("bind-pr-standalone");
        store
            .enqueue_command(&instance.id, None, &standalone)
            .await?;
        assert_eq!(
            store
                .repair_pr_binding_from_latest_command(&instance.id, 0, "test")
                .await?,
            WorkflowPrBindingRepairOutcome::NoBindPrCommand
        );

        // Neither is a command whose decision was rejected.
        let rejected_command = bind_pr("bind-pr-rejected");
        let rejected = WorkflowDecisionRecord::rejected(
            bind_pr_decision(&instance.id, &rejected_command),
            None,
            "transition is outside the allowlist",
        );
        store.record_decision(&rejected).await?;
        store
            .enqueue_command(&instance.id, Some(&rejected.id), &rejected_command)
            .await?;
        assert_eq!(
            store
                .repair_pr_binding_from_latest_command(&instance.id, 0, "test")
                .await?,
            WorkflowPrBindingRepairOutcome::NoBindPrCommand
        );
        assert!(
            repair_events(&store, &instance.id).await?.is_empty(),
            "a refused repair must not record provenance"
        );
        Ok(())
    }

    #[tokio::test]
    async fn pr_binding_repair_binds_accepted_evidence_and_records_event() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let instance = workflow("pr-binding-repair-accepted");
        store
            .force_upsert_lifecycle_state_for_test(&instance)
            .await?;

        let command = bind_pr("bind-pr-1845");
        let record =
            WorkflowDecisionRecord::accepted(bind_pr_decision(&instance.id, &command), None);
        store.record_decision(&record).await?;
        let command_id = store
            .enqueue_command(&instance.id, Some(&record.id), &command)
            .await?;

        let repaired = store
            .repair_pr_binding_from_latest_command(&instance.id, 0, "test")
            .await?;
        let WorkflowPrBindingRepairOutcome::Repaired {
            instance: repaired,
            command_id: repaired_command_id,
            pr_number,
            pr_url,
        } = repaired
        else {
            anyhow::bail!("bound bind_pr evidence should repair the missing binding");
        };
        assert_eq!(repaired_command_id, command_id);
        assert_eq!(pr_number, 1845);
        assert_eq!(pr_url, "https://github.com/majiayu000/harness/pull/1845");
        assert_eq!(repaired.version, 1);
        assert_eq!(repaired.data["pr_number"], 1845);

        let events = repair_events(&store, &instance.id).await?;
        assert_eq!(events.len(), 1, "a repair must record exactly one event");
        let payload = &events[0].event;
        assert_eq!(payload["command_id"], json!(command_id));
        assert_eq!(payload["decision_id"], json!(record.id));
        assert_eq!(payload["pr_number"], json!(1845));
        assert_eq!(events[0].source, "test");

        assert_eq!(
            store
                .repair_pr_binding_from_latest_command(&instance.id, 0, "test")
                .await?,
            WorkflowPrBindingRepairOutcome::StaleInstance
        );
        Ok(())
    }

    #[tokio::test]
    async fn pr_binding_repair_ignores_superseded_evidence() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let instance = workflow("pr-binding-repair-superseded");
        store
            .force_upsert_lifecycle_state_for_test(&instance)
            .await?;

        let command = bind_pr("bind-pr-superseded");
        let record =
            WorkflowDecisionRecord::accepted(bind_pr_decision(&instance.id, &command), None);
        store.record_decision(&record).await?;
        store
            .enqueue_command(&instance.id, Some(&record.id), &command)
            .await?;

        // A newer decision reuses the dedupe key for different work,
        // superseding the bind_pr attempt (GH-1865 W2).
        let replacement = WorkflowCommand::enqueue_activity("implement", "bind-pr-superseded");
        let replacement_record = WorkflowDecisionRecord::accepted(
            WorkflowDecision::new(
                &instance.id,
                "awaiting_feedback",
                "schedule_rework",
                "awaiting_feedback",
                "rework scheduled over the bind attempt",
            )
            .with_command(replacement.clone()),
            None,
        );
        store.record_decision(&replacement_record).await?;
        store
            .enqueue_command(&instance.id, Some(&replacement_record.id), &replacement)
            .await?;

        assert_eq!(
            store
                .repair_pr_binding_from_latest_command(&instance.id, 0, "test")
                .await?,
            WorkflowPrBindingRepairOutcome::NoBindPrCommand,
            "a superseded bind_pr attempt is history, not evidence"
        );
        Ok(())
    }
}
