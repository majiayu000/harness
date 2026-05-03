use super::model::{
    ActivityResult, ActivityStatus, RuntimeJob, RuntimeProfile, WorkflowCommandRecord,
    WorkflowCommandType,
};
use super::reducer::reduce_runtime_job_completed;
use super::store::WorkflowRuntimeStore;
use super::validator::{DecisionValidator, ValidationContext};
use anyhow::Context;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::json;

#[async_trait]
pub trait RuntimeJobExecutor: Send + Sync {
    fn consumes_runtime_turn(&self, _job: &RuntimeJob) -> bool {
        true
    }

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

        let consumes_runtime_turn = executor.consumes_runtime_turn(&job);
        let result = match self
            .max_turns_budget_result(&job, consumes_runtime_turn)
            .await?
        {
            Some(result) => result,
            None => {
                if consumes_runtime_turn {
                    self.store
                        .record_runtime_event(
                            &job.id,
                            "RuntimeTurnStarted",
                            json!({
                                "owner": self.owner.as_str(),
                            }),
                        )
                        .await?;
                }
                executor.execute(job.clone()).await
            }
        };
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
        let active_start_child_workflow_commands =
            if command.command.command_type == WorkflowCommandType::StartChildWorkflow {
                self.active_start_child_workflow_commands(&command.workflow_id)
                    .await?
            } else {
                0
            };
        let event = self
            .store
            .append_event(
                &command.workflow_id,
                "RuntimeJobCompleted",
                &self.owner,
                json!({
                    "command_id": command.id,
                    "command": command.command,
                    "runtime_job_id": job.id,
                    "runtime_job_status": job.status,
                    "active_start_child_workflow_commands": active_start_child_workflow_commands,
                    "activity_result": result,
                }),
            )
            .await?;
        self.apply_runtime_completion_reducer(&command.workflow_id, &event)
            .await?;
        self.propagate_pr_feedback_child_completion(&command.workflow_id, &event)
            .await?;
        Ok(())
    }

    async fn active_start_child_workflow_commands(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<usize> {
        Ok(self
            .store
            .commands_for(workflow_id)
            .await?
            .into_iter()
            .filter(|record| {
                record.command.command_type == WorkflowCommandType::StartChildWorkflow
                    && matches!(record.status.as_str(), "pending" | "dispatched")
            })
            .count())
    }

    async fn propagate_pr_feedback_child_completion(
        &self,
        workflow_id: &str,
        event: &super::model::WorkflowEvent,
    ) -> anyhow::Result<()> {
        let Some(child) = self.store.get_instance(workflow_id).await? else {
            return Ok(());
        };
        if child.definition_id != super::pr_feedback::PR_FEEDBACK_DEFINITION_ID {
            return Ok(());
        }
        if !runtime_event_result_succeeded(event) {
            return Ok(());
        }
        if matches!(child.state.as_str(), "pending" | "inspecting") {
            return Ok(());
        }
        let Some(parent_workflow_id) = child.parent_workflow_id.as_deref() else {
            return Ok(());
        };
        let parent_event = self
            .store
            .append_event(
                parent_workflow_id,
                "RuntimeJobCompleted",
                &self.owner,
                merge_child_completion_payload(event, &child.id),
            )
            .await?;
        self.apply_runtime_completion_reducer(parent_workflow_id, &parent_event)
            .await
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
            super::quality_gate::QUALITY_GATE_DEFINITION_ID => DecisionValidator::quality_gate(),
            super::pr_feedback::PR_FEEDBACK_DEFINITION_ID => DecisionValidator::pr_feedback(),
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
                    error.to_string(),
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

    async fn max_turns_budget_result(
        &self,
        job: &RuntimeJob,
        consumes_runtime_turn: bool,
    ) -> anyhow::Result<Option<ActivityResult>> {
        if !consumes_runtime_turn {
            return Ok(None);
        }
        let Some(profile) = runtime_profile_for_job(job)? else {
            return Ok(None);
        };
        let Some(max_turns) = profile.max_turns else {
            return Ok(None);
        };
        let Some(command) = self.store.get_command(&job.command_id).await? else {
            return Ok(None);
        };
        let turns_started = self
            .store
            .runtime_turns_started_for_workflow(&command.workflow_id, Some(&job.id))
            .await?;
        if turns_started < i64::from(max_turns) {
            return Ok(None);
        }
        Ok(Some(runtime_budget_blocked_result(
            &command,
            &profile,
            turns_started,
            max_turns,
        )))
    }
}

fn runtime_event_result_succeeded(event: &super::model::WorkflowEvent) -> bool {
    event
        .event
        .get("activity_result")
        .cloned()
        .and_then(|value| serde_json::from_value::<ActivityResult>(value).ok())
        .is_some_and(|result| result.status == ActivityStatus::Succeeded)
}

fn merge_child_completion_payload(
    event: &super::model::WorkflowEvent,
    child_workflow_id: &str,
) -> serde_json::Value {
    let mut payload = event.event.clone();
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "child_workflow_id".to_string(),
            serde_json::json!(child_workflow_id),
        );
        if let Some(artifacts) = object
            .get_mut("activity_result")
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|activity_result| activity_result.get_mut("artifacts"))
            .and_then(serde_json::Value::as_array_mut)
        {
            artifacts.retain(|artifact| {
                artifact
                    .get("artifact_type")
                    .and_then(serde_json::Value::as_str)
                    != Some("workflow_decision")
            });
        }
    }
    payload
}

fn command_status_for_activity(status: ActivityStatus) -> &'static str {
    match status {
        ActivityStatus::Succeeded => "completed",
        ActivityStatus::Failed => "failed",
        ActivityStatus::Blocked => "blocked",
        ActivityStatus::Cancelled => "cancelled",
    }
}

fn runtime_profile_for_job(job: &RuntimeJob) -> anyhow::Result<Option<RuntimeProfile>> {
    let Some(value) = job.input.get("runtime_profile") else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .with_context(|| format!("runtime job {} has invalid runtime_profile input", job.id))
        .map(Some)
}

fn runtime_budget_blocked_result(
    command: &WorkflowCommandRecord,
    profile: &RuntimeProfile,
    turns_started: i64,
    max_turns: u32,
) -> ActivityResult {
    let activity = command.command.runtime_activity_key().to_string();
    let error = format!(
        "Runtime profile `{}` exhausted max_turns: used {} of {} allowed runtime turns",
        profile.name, turns_started, max_turns
    );
    ActivityResult {
        activity,
        status: ActivityStatus::Blocked,
        summary: "Runtime turn budget exhausted before execution.".to_string(),
        artifacts: Vec::new(),
        signals: Vec::new(),
        validation: Vec::new(),
        error: Some(error),
        error_kind: None,
    }
}
