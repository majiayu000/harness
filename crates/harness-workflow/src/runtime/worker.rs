use super::model::{
    ActivityResult, ActivityStatus, RuntimeJob, RuntimeKind, RuntimeProfile, WorkflowCommand,
    WorkflowCommandRecord, WorkflowInstance,
};
use super::store::WorkflowRuntimeStore;
use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::time::Duration as StdDuration;

#[async_trait]
pub trait RuntimeJobExecutor: Send + Sync {
    fn consumes_runtime_turn(&self, _job: &RuntimeJob) -> bool {
        true
    }

    async fn preflight_result(&self, _job: &RuntimeJob) -> Option<ActivityResult> {
        None
    }

    async fn execute(&self, job: RuntimeJob) -> ActivityResult;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeJobClaimDecision {
    Proceed,
    Defer {
        not_before: DateTime<Utc>,
        reason: String,
    },
}

pub trait RuntimeJobClaimGuard: Send + Sync {
    fn before_execute(
        &self,
        job: &RuntimeJob,
        now: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> RuntimeJobClaimDecision;
}

pub struct RuntimeWorker<'a> {
    store: &'a WorkflowRuntimeStore,
    owner: String,
    lease_ttl: Duration,
    claim_guard: Option<&'a (dyn RuntimeJobClaimGuard + Send + Sync)>,
}

impl<'a> RuntimeWorker<'a> {
    pub fn new(store: &'a WorkflowRuntimeStore, owner: impl Into<String>) -> Self {
        Self {
            store,
            owner: owner.into(),
            lease_ttl: Duration::minutes(15),
            claim_guard: None,
        }
    }

    pub fn with_lease_ttl(mut self, lease_ttl: Duration) -> Self {
        self.lease_ttl = lease_ttl;
        self
    }

    pub fn with_claim_guard(
        mut self,
        claim_guard: &'a (dyn RuntimeJobClaimGuard + Send + Sync),
    ) -> Self {
        self.claim_guard = Some(claim_guard);
        self
    }

    pub async fn run_once(
        &self,
        executor: &(dyn RuntimeJobExecutor + Send + Sync),
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut lease_expires_at = Utc::now() + self.lease_ttl;
        let Some(job) = self
            .store
            .claim_next_runtime_job_excluding_runtime_kind(
                RuntimeKind::RemoteHost,
                &self.owner,
                lease_expires_at,
            )
            .await?
        else {
            return Ok(None);
        };

        if let Some(claim_guard) = self.claim_guard {
            match claim_guard.before_execute(&job, Utc::now(), lease_expires_at) {
                RuntimeJobClaimDecision::Proceed => {}
                RuntimeJobClaimDecision::Defer { not_before, reason } => {
                    let Some(_) = self
                        .store
                        .defer_runtime_job_claim_if_owned(
                            &job.id,
                            &self.owner,
                            lease_expires_at,
                            not_before,
                        )
                        .await?
                    else {
                        tracing::warn!(
                            runtime_job_id = %job.id,
                            owner = %self.owner,
                            "runtime job claim defer ignored because the worker no longer owns the lease"
                        );
                        return Ok(None);
                    };
                    self.store
                        .record_runtime_event(
                            &job.id,
                            "RuntimeJobClaimDeferred",
                            json!({
                                "owner": self.owner.as_str(),
                                "not_before": not_before,
                                "reason": reason,
                            }),
                        )
                        .await?;
                    return Ok(None);
                }
            }
        }

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
                    None => match executor.preflight_result(&job).await {
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
                    },
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
            self.propagate_quality_gate_child_completion(&event.workflow_id, event)
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
        let activity = runtime_job_activity_name(job);
        let summary = format!(
            "Workflow {} was already terminal ({}) before runtime execution.",
            instance.id, instance.state
        );
        let result = match instance.state.as_str() {
            "cancelled" => ActivityResult::cancelled(activity, summary),
            "failed" => ActivityResult::failed(activity, summary, "workflow already failed"),
            _ => ActivityResult::succeeded(activity, summary),
        };
        Ok(Some(result))
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
        self.store
            .commit_parent_runtime_completion(
                parent_workflow_id,
                &self.owner,
                merge_child_completion_payload(event, &child.id),
            )
            .await?;
        Ok(())
    }

    async fn propagate_quality_gate_child_completion(
        &self,
        workflow_id: &str,
        event: &super::model::WorkflowEvent,
    ) -> anyhow::Result<()> {
        let Some(child) = self.store.get_instance(workflow_id).await? else {
            return Ok(());
        };
        if child.definition_id != super::quality_gate::QUALITY_GATE_DEFINITION_ID {
            return Ok(());
        }
        if !matches!(
            child.state.as_str(),
            "passed" | "blocked" | "failed" | "cancelled"
        ) {
            return Ok(());
        }
        let Some(parent_workflow_id) = child.parent_workflow_id.as_deref() else {
            return Ok(());
        };
        self.store
            .commit_parent_runtime_completion(
                parent_workflow_id,
                &self.owner,
                merge_child_completion_payload(event, &child.id),
            )
            .await?;
        Ok(())
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

/// Persist stopped-state metadata into instance data for operator surfaces.
pub(super) fn apply_failure_reason_side_effect(
    instance: &mut WorkflowInstance,
    command: &WorkflowCommand,
) -> anyhow::Result<()> {
    let reason = command
        .command
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty());
    if reason.is_none()
        && !STOP_STRING_FIELDS
            .iter()
            .any(|field| command_string_field(command, field).is_some())
        && command.command.get("last_stop").is_none()
    {
        return Ok(());
    }
    if !instance.data.is_object() {
        instance.data = json!({});
    }
    let data = instance
        .data
        .as_object_mut()
        .context("workflow instance data is not an object")?;
    if let Some(reason) = reason {
        data.insert("failure_reason".to_string(), json!(reason));
        if command.command_type == super::model::WorkflowCommandType::MarkBlocked {
            data.insert("blocked_reason".to_string(), json!(reason));
        }
    }
    for field in STOP_STRING_FIELDS {
        if let Some(value) = command_string_field(command, field) {
            data.insert((*field).to_string(), json!(value));
        }
    }
    if let Some(last_stop) = command.command.get("last_stop") {
        data.insert("last_stop".to_string(), last_stop.clone());
    }
    Ok(())
}

const STOP_STRING_FIELDS: &[&str] = &[
    "blocked_reason",
    "unblock_hint",
    "failure_reason",
    "retry_hint",
    "error_kind",
    "stop_reason_code",
    "reason_class",
];

fn command_string_field<'a>(command: &'a WorkflowCommand, field: &str) -> Option<&'a str> {
    command
        .command
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
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

#[cfg(test)]
mod tests;
