use super::model::{ActivityResult, ActivityStatus, RuntimeJob};
use super::reducer::reduce_runtime_job_completed;
use super::store::WorkflowRuntimeStore;
use super::validator::{DecisionValidator, ValidationContext};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::json;

#[async_trait]
pub trait RuntimeJobExecutor: Send + Sync {
    async fn execute(&self, job: RuntimeJob) -> ActivityResult;
}

pub struct RuntimeWorker<'a> {
    store: &'a WorkflowRuntimeStore,
    owner: String,
    lease_ttl: Duration,
}

impl<'a> RuntimeWorker<'a> {
    pub fn new(store: &'a WorkflowRuntimeStore, owner: impl Into<String>) -> Self {
        Self {
            store,
            owner: owner.into(),
            lease_ttl: Duration::minutes(15),
        }
    }

    pub fn with_lease_ttl(mut self, lease_ttl: Duration) -> Self {
        self.lease_ttl = lease_ttl;
        self
    }

    pub async fn run_once(
        &self,
        executor: &(dyn RuntimeJobExecutor + Send + Sync),
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let lease_expires_at = Utc::now() + self.lease_ttl;
        let Some(job) = self
            .store
            .claim_next_runtime_job(&self.owner, lease_expires_at)
            .await?
        else {
            return Ok(None);
        };

        self.store
            .record_runtime_event(
                &job.id,
                "RuntimeJobClaimed",
                json!({
                    "owner": self.owner.as_str(),
                    "lease_expires_at": lease_expires_at,
                }),
            )
            .await?;

        let result = executor.execute(job.clone()).await;
        self.store
            .record_runtime_event(
                &job.id,
                "ActivityResultReady",
                serde_json::to_value(&result)?,
            )
            .await?;
        let completed = self.store.complete_runtime_job(&job.id, &result).await?;
        self.record_workflow_completion(&completed, &result).await?;
        Ok(Some(completed))
    }

    async fn record_workflow_completion(
        &self,
        job: &RuntimeJob,
        result: &ActivityResult,
    ) -> anyhow::Result<()> {
        let Some(command) = self.store.get_command(&job.command_id).await? else {
            return Ok(());
        };
        self.store
            .mark_command_status(&command.id, command_status_for_activity(result.status))
            .await?;
        let event = self
            .store
            .append_event(
                &command.workflow_id,
                "RuntimeJobCompleted",
                &self.owner,
                json!({
                    "command_id": command.id,
                    "runtime_job_id": job.id,
                    "runtime_job_status": job.status,
                    "activity_result": result,
                }),
            )
            .await?;
        self.apply_runtime_completion_reducer(&command.workflow_id, &event)
            .await?;
        Ok(())
    }

    async fn apply_runtime_completion_reducer(
        &self,
        workflow_id: &str,
        event: &super::model::WorkflowEvent,
    ) -> anyhow::Result<()> {
        let Some(mut instance) = self.store.get_instance(workflow_id).await? else {
            return Ok(());
        };
        let Some(decision) = reduce_runtime_job_completed(&instance, event)? else {
            return Ok(());
        };

        let validator = match instance.definition_id.as_str() {
            super::reducer::GITHUB_ISSUE_PR_DEFINITION_ID => DecisionValidator::github_issue_pr(),
            super::repo_backlog::REPO_BACKLOG_DEFINITION_ID => DecisionValidator::repo_backlog(),
            _ => return Ok(()),
        };
        let validation = validator.validate(
            &instance,
            &decision,
            &ValidationContext::new(&self.owner, Utc::now()),
        );
        let record = match validation {
            Ok(()) => super::model::WorkflowDecisionRecord::accepted(
                decision.clone(),
                Some(event.id.clone()),
            ),
            Err(error) => {
                let record = super::model::WorkflowDecisionRecord::rejected(
                    decision,
                    Some(event.id.clone()),
                    &error.to_string(),
                );
                self.store.record_decision(&record).await?;
                return Ok(());
            }
        };

        self.store.record_decision(&record).await?;
        for command in &decision.commands {
            self.store
                .enqueue_command(&instance.id, Some(&record.id), command)
                .await?;
        }
        instance.state = decision.next_state.clone();
        instance.version = instance.version.saturating_add(1);
        self.store.upsert_instance(&instance).await
    }
}

fn command_status_for_activity(status: ActivityStatus) -> &'static str {
    match status {
        ActivityStatus::Succeeded => "completed",
        ActivityStatus::Failed => "failed",
        ActivityStatus::Blocked => "blocked",
        ActivityStatus::Cancelled => "cancelled",
    }
}
