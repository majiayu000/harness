use super::{
    command_store, insert_decision_record_tx, insert_event_tx_with_id,
    insert_instance_if_absent_tx, runtime_job_state::cancel_unfinished_runtime_jobs_tx,
    select_instance_for_update_tx, upsert_instance_tx, WorkflowRuntimeStore,
};
use crate::runtime::remote_facts::upsert_remote_fact_snapshot_tx;
use crate::runtime::{
    RemoteFactSnapshot, WorkflowCommandStatus, WorkflowDecision, WorkflowDecisionRecord,
    WorkflowInstance,
};
use harness_core::config::isolation::IsolationTrustClass;
use serde_json::Value;

pub enum WorkflowCoverageRecoveryExpected<'a> {
    Absent,
    Present { state: &'a str, version: u64 },
}

pub struct WorkflowCoverageRecoveryTransition<'a> {
    pub workflow_id: &'a str,
    pub expected: WorkflowCoverageRecoveryExpected<'a>,
    pub final_instance: &'a WorkflowInstance,
    pub selected_remote_fact: &'a RemoteFactSnapshot,
    pub event_type: &'a str,
    pub source: &'a str,
    pub payload: Value,
    pub decision: &'a WorkflowDecision,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowCoverageRecoveryOutcome {
    Committed {
        command_ids: Vec<String>,
    },
    Conflict {
        current: Option<Box<WorkflowInstance>>,
    },
    StaleRemoteFact,
}

impl WorkflowRuntimeStore {
    pub async fn reconcile_instance_author_trust_class(
        &self,
        workflow_id: &str,
        incoming: IsolationTrustClass,
    ) -> anyhow::Result<Option<WorkflowInstance>> {
        let mut tx = self.pool.begin().await?;
        let Some(mut instance) = select_instance_for_update_tx(&mut tx, workflow_id).await? else {
            tx.commit().await?;
            return Ok(None);
        };
        let current = instance
            .data
            .get("author_trust_class")
            .map(|value| {
                serde_json::from_value::<IsolationTrustClass>(value.clone()).map_err(|error| {
                    anyhow::anyhow!(
                        "invalid durable author_trust_class for workflow `{workflow_id}`: {error}"
                    )
                })
            })
            .transpose()?;
        let effective = if current == Some(IsolationTrustClass::NonCollaborator)
            || incoming == IsolationTrustClass::NonCollaborator
        {
            IsolationTrustClass::NonCollaborator
        } else {
            IsolationTrustClass::Trusted
        };
        if current != Some(effective) {
            let data = instance.data.as_object_mut().ok_or_else(|| {
                anyhow::anyhow!("workflow `{workflow_id}` data must be an object")
            })?;
            data.insert(
                "author_trust_class".to_string(),
                serde_json::to_value(effective)?,
            );
            instance.version = instance.version.saturating_add(1);
            upsert_instance_tx(&mut tx, &instance).await?;
        }
        tx.commit().await?;
        Ok(Some(instance))
    }

    pub async fn commit_coverage_recovery_transition(
        &self,
        transition: WorkflowCoverageRecoveryTransition<'_>,
    ) -> anyhow::Result<WorkflowCoverageRecoveryOutcome> {
        if transition.final_instance.id != transition.workflow_id
            || transition.decision.workflow_id != transition.workflow_id
        {
            anyhow::bail!("coverage recovery workflow identifiers do not match");
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("workflow_submission:{}", transition.workflow_id))
            .execute(&mut *tx)
            .await?;
        let current = select_instance_for_update_tx(&mut tx, transition.workflow_id).await?;
        let expected_matches = match (&transition.expected, current.as_ref()) {
            (WorkflowCoverageRecoveryExpected::Absent, None) => true,
            (WorkflowCoverageRecoveryExpected::Present { state, version }, Some(current)) => {
                current.state == *state && current.version == *version
            }
            _ => false,
        };
        if !expected_matches {
            tx.rollback().await?;
            return Ok(WorkflowCoverageRecoveryOutcome::Conflict {
                current: current.map(Box::new),
            });
        }
        let expected_version = current.as_ref().map_or(0, |instance| instance.version + 1);
        if transition.final_instance.version != expected_version {
            anyhow::bail!("coverage recovery final instance has an invalid version");
        }
        if current.is_none()
            && !insert_instance_if_absent_tx(&mut tx, transition.final_instance).await?
        {
            anyhow::bail!("coverage recovery lost its advisory-locked absent insert");
        }

        let persisted_fact =
            upsert_remote_fact_snapshot_tx(&mut tx, transition.selected_remote_fact).await?;
        if persisted_fact.fact_hash != transition.selected_remote_fact.fact_hash {
            tx.rollback().await?;
            return Ok(WorkflowCoverageRecoveryOutcome::StaleRemoteFact);
        }
        let event = insert_event_tx_with_id(
            &mut tx,
            transition.workflow_id,
            transition.event_type,
            transition.source,
            transition.payload,
            None,
        )
        .await?;
        let record = WorkflowDecisionRecord::accepted(transition.decision.clone(), Some(event.id));
        insert_decision_record_tx(&mut tx, &record).await?;

        let desired_keys = transition
            .decision
            .commands
            .iter()
            .map(|command| command.dedupe_key.as_str())
            .collect::<Vec<_>>();
        let active: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, data::text FROM workflow_commands
             WHERE workflow_id = $1 AND status IN ('pending', 'dispatching', 'dispatched', 'deferred')
             FOR UPDATE",
        )
        .bind(transition.workflow_id)
        .fetch_all(&mut *tx)
        .await?;
        for (command_id, data) in active {
            let command: crate::runtime::WorkflowCommand = serde_json::from_str(&data)?;
            if desired_keys.contains(&command.dedupe_key.as_str()) {
                continue;
            }
            cancel_unfinished_runtime_jobs_tx(
                &mut tx,
                &command_id,
                "github_coverage_recovery",
                "GitHub reported an existing closing pull request.",
            )
            .await?;
            sqlx::query(
                "UPDATE workflow_commands SET status = 'cancelled', dispatch_owner = NULL,
                    dispatch_lease_expires_at = NULL, dispatch_not_before = NULL,
                    dispatch_barrier = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = $1",
            )
            .bind(command_id)
            .execute(&mut *tx)
            .await?;
        }

        let mut command_ids = Vec::new();
        for command in &transition.decision.commands {
            command_ids.push(
                command_store::insert_or_reactivate_cancelled_tx(
                    &mut tx,
                    transition.workflow_id,
                    Some(&record.id),
                    command,
                    WorkflowCommandStatus::Pending,
                )
                .await?,
            );
        }
        upsert_instance_tx(&mut tx, transition.final_instance).await?;
        tx.commit().await?;
        Ok(WorkflowCoverageRecoveryOutcome::Committed { command_ids })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{WorkflowCommand, WorkflowSubject};
    use chrono::Utc;
    use harness_core::db::resolve_database_url;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Barrier;

    #[tokio::test]
    async fn concurrent_recovery_attempts_have_one_atomic_winner() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let store_ref = &store;
        let barrier = Arc::new(Barrier::new(2));
        let run = |suffix: &'static str, barrier: Arc<Barrier>| {
            let final_instance = WorkflowInstance::new(
                "github_issue_pr",
                1,
                "quality_gate_pending",
                WorkflowSubject::new("issue", "issue:1707"),
            )
            .with_id("coverage-race")
            .with_data(json!({"winner": suffix}));
            let fact = RemoteFactSnapshot::new(
                "github",
                "owner/repo",
                "pull_request",
                1709,
                "open",
                json!({"head": "abc"}),
                Utc::now(),
            );
            let decision = WorkflowDecision::new(
                "coverage-race",
                "missing",
                "recover",
                "quality_gate_pending",
                "recover",
            )
            .with_command(WorkflowCommand::enqueue_activity(
                "quality_gate",
                "quality-gate:1709",
            ));
            async move {
                barrier.wait().await;
                store_ref
                    .commit_coverage_recovery_transition(WorkflowCoverageRecoveryTransition {
                        workflow_id: "coverage-race",
                        expected: WorkflowCoverageRecoveryExpected::Absent,
                        final_instance: &final_instance,
                        selected_remote_fact: &fact,
                        event_type: "ClosingPrCoverageRecovered",
                        source: "test",
                        payload: json!({"winner": suffix}),
                        decision: &decision,
                    })
                    .await
            }
        };
        let (left, right) = tokio::join!(run("left", barrier.clone()), run("right", barrier));
        let outcomes = [left?, right?];
        assert_eq!(
            outcomes
                .iter()
                .filter(|value| matches!(value, WorkflowCoverageRecoveryOutcome::Committed { .. }))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|value| matches!(value, WorkflowCoverageRecoveryOutcome::Conflict { .. }))
                .count(),
            1
        );
        assert_eq!(store.commands_for("coverage-race").await?.len(), 1);
        assert_eq!(store.events_for("coverage-race").await?.len(), 1);
        assert_eq!(store.decisions_for("coverage-race").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn stale_recovery_cannot_cancel_newer_transition_command() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let initial = WorkflowInstance::new(
            "github_issue_pr",
            1,
            "planning",
            WorkflowSubject::new("issue", "issue:1707"),
        )
        .with_id("coverage-stale");
        store.upsert_instance(&initial).await?;
        let newer_command = WorkflowCommand::enqueue_activity("implement_issue", "newer-command");
        let newer_command_id = store
            .enqueue_command(&initial.id, None, &newer_command)
            .await?;
        let mut newer = initial.clone();
        newer.state = "implementing".to_string();
        newer.version = 1;
        newer.data = json!({"newer": true});
        store.upsert_instance(&newer).await?;

        let mut stale_final = initial.clone();
        stale_final.state = "pr_open".to_string();
        stale_final.version = 1;
        let fact = RemoteFactSnapshot::new(
            "github",
            "owner/repo",
            "pull_request",
            1709,
            "open",
            json!({"head": "old"}),
            Utc::now(),
        );
        let decision =
            WorkflowDecision::new(&initial.id, "planning", "recover", "pr_open", "recover");
        let outcome = store
            .commit_coverage_recovery_transition(WorkflowCoverageRecoveryTransition {
                workflow_id: &initial.id,
                expected: WorkflowCoverageRecoveryExpected::Present {
                    state: "planning",
                    version: 0,
                },
                final_instance: &stale_final,
                selected_remote_fact: &fact,
                event_type: "ClosingPrCoverageRecovered",
                source: "test",
                payload: json!({}),
                decision: &decision,
            })
            .await?;
        assert!(matches!(
            outcome,
            WorkflowCoverageRecoveryOutcome::Conflict { .. }
        ));
        assert_eq!(
            store
                .get_instance(&initial.id)
                .await?
                .expect("workflow")
                .state,
            "implementing"
        );
        assert_eq!(
            store
                .get_command(&newer_command_id)
                .await?
                .expect("command")
                .status,
            WorkflowCommandStatus::Pending
        );
        assert!(store
            .get_remote_fact_snapshot("github", "owner/repo", "pull_request", 1709)
            .await?
            .is_none());
        Ok(())
    }
}
