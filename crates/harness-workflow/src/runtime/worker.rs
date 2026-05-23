use super::model::{
    ActivityResult, ActivityStatus, RuntimeJob, RuntimeProfile, WorkflowCommand,
    WorkflowCommandRecord, WorkflowCommandType, WorkflowInstance,
};
use super::reducer::reduce_runtime_job_completed;
use super::store::WorkflowRuntimeStore;
use super::validator::{DecisionValidator, ValidationContext};
use anyhow::Context;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use std::time::Duration as StdDuration;

const COMMAND_STATUS_HANDLED_INLINE: &str = "handled_inline";

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
        let mut lease_expires_at = Utc::now() + self.lease_ttl;
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
        let result = match self.terminal_workflow_result(&job).await? {
            Some(result) => result,
            None => {
                match self
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
                        let execution = self
                            .execute_with_lease_renewal(&job, executor, lease_expires_at)
                            .await?;
                        lease_expires_at = execution.lease_expires_at;
                        execution.result
                    }
                }
            }
        };
        self.store
            .record_runtime_event(
                &job.id,
                "ActivityResultReady",
                serde_json::to_value(&result)?,
            )
            .await?;
        let Some(completion) = self
            .store
            .commit_runtime_activity_completion_if_owned(
                &job.id,
                &self.owner,
                lease_expires_at,
                &result,
            )
            .await?
        else {
            tracing::warn!(
                runtime_job_id = %job.id,
                owner = %self.owner,
                "runtime job completion ignored because the worker no longer owns the lease"
            );
            return Ok(None);
        };
        if let Some(event) = completion.workflow_event.as_ref() {
            self.propagate_pr_feedback_child_completion(&event.workflow_id, event)
                .await?;
        }
        Ok(Some(completion.runtime_job))
    }

    async fn terminal_workflow_result(
        &self,
        job: &RuntimeJob,
    ) -> anyhow::Result<Option<ActivityResult>> {
        let Some(workflow_id) = job.input.get("workflow_id").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some(instance) = self.store.get_instance(workflow_id).await? else {
            return Ok(None);
        };
        if !instance.is_terminal() {
            return Ok(None);
        }
        Ok(Some(ActivityResult::cancelled(
            runtime_job_activity_name(job),
            format!(
                "Workflow {} was already terminal ({}) before runtime execution.",
                instance.id, instance.state
            ),
        )))
    }

    async fn execute_with_lease_renewal(
        &self,
        job: &RuntimeJob,
        executor: &(dyn RuntimeJobExecutor + Send + Sync),
        initial_lease_expires_at: chrono::DateTime<Utc>,
    ) -> anyhow::Result<RuntimeJobExecution> {
        let mut lease_expires_at = initial_lease_expires_at;
        let renewal_interval = runtime_lease_renewal_interval(self.lease_ttl);
        let activity = runtime_job_activity_name(job);
        let execution = executor.execute(job.clone());
        tokio::pin!(execution);

        loop {
            let renewal_sleep = tokio::time::sleep(renewal_interval);
            tokio::pin!(renewal_sleep);

            tokio::select! {
                result = &mut execution => {
                    return Ok(RuntimeJobExecution {
                        result,
                        lease_expires_at,
                    });
                }
                _ = &mut renewal_sleep => {
                    let next_lease_expires_at = Utc::now() + self.lease_ttl;
                    let Some(updated) = self.store
                        .extend_runtime_job_lease_if_owned(
                            &job.id,
                            &self.owner,
                            lease_expires_at,
                            next_lease_expires_at,
                        )
                        .await?
                    else {
                        return Ok(RuntimeJobExecution {
                            result: ActivityResult::failed(
                                activity,
                                "Runtime job lease was lost before the agent completed.",
                                "Another runtime worker reclaimed the job after this worker's lease expired.",
                            ),
                            lease_expires_at,
                        });
                    };
                    lease_expires_at = updated
                        .lease
                        .as_ref()
                        .map(|lease| lease.expires_at)
                        .unwrap_or(next_lease_expires_at);
                }
            }
        }
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
            super::prompt_task::PROMPT_TASK_DEFINITION_ID => DecisionValidator::prompt_task(),
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
            if command.requires_runtime_job() {
                self.store
                    .enqueue_command(&instance.id, Some(&record.id), command)
                    .await?;
            } else {
                self.store
                    .enqueue_command_with_status(
                        &instance.id,
                        Some(&record.id),
                        command,
                        COMMAND_STATUS_HANDLED_INLINE,
                    )
                    .await?;
                apply_inline_command_side_effect(&mut instance, command)?;
            }
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

struct RuntimeJobExecution {
    result: ActivityResult,
    lease_expires_at: chrono::DateTime<Utc>,
}

fn runtime_lease_renewal_interval(lease_ttl: Duration) -> StdDuration {
    let lease_ttl_secs = lease_ttl.num_seconds().max(1) as u64;
    StdDuration::from_secs((lease_ttl_secs / 2).clamp(1, 30))
}

fn runtime_job_activity_name(job: &RuntimeJob) -> String {
    job.input
        .get("activity")
        .and_then(Value::as_str)
        .unwrap_or("runtime_job")
        .to_string()
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

fn apply_inline_command_side_effect(
    instance: &mut WorkflowInstance,
    command: &WorkflowCommand,
) -> anyhow::Result<()> {
    match command.command_type {
        WorkflowCommandType::BindPr => apply_bind_pr_side_effect(instance, command),
        WorkflowCommandType::MarkDone => apply_mark_done_side_effect(instance, command),
        _ => Ok(()),
    }
}

fn apply_bind_pr_side_effect(
    instance: &mut WorkflowInstance,
    command: &WorkflowCommand,
) -> anyhow::Result<()> {
    let pr_number = command
        .command
        .get("pr_number")
        .and_then(Value::as_u64)
        .context("bind_pr command missing pr_number")?;
    let pr_url = command
        .command
        .get("pr_url")
        .and_then(Value::as_str)
        .context("bind_pr command missing pr_url")?;

    if !instance.data.is_object() {
        instance.data = json!({});
    }
    let data = instance
        .data
        .as_object_mut()
        .context("workflow instance data is not an object")?;
    data.insert("pr_number".to_string(), json!(pr_number));
    data.insert("pr_url".to_string(), json!(pr_url));
    Ok(())
}

fn apply_mark_done_side_effect(
    instance: &mut WorkflowInstance,
    command: &WorkflowCommand,
) -> anyhow::Result<()> {
    let Some(closed_issue_evidence) = command.command.get("closed_issue_evidence").cloned() else {
        return Ok(());
    };
    if !instance.data.is_object() {
        instance.data = json!({});
    }
    let data = instance
        .data
        .as_object_mut()
        .context("workflow instance data is not an object")?;
    data.insert("closed_issue_evidence".to_string(), closed_issue_evidence);
    Ok(())
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
