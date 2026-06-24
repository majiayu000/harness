use super::model::{
    ActivityResult, ActivityStatus, RuntimeJob, RuntimeKind, RuntimeProfile, WorkflowCommand,
    WorkflowCommandRecord, WorkflowInstance,
};
use super::store::WorkflowRuntimeStore;
use anyhow::Context;
use async_trait::async_trait;
use chrono::{Duration, Utc};
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
            .claim_next_runtime_job_excluding_runtime_kind(
                RuntimeKind::RemoteHost,
                &self.owner,
                lease_expires_at,
            )
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

/// Persist the terminal-failure `reason` into the instance `data` under
/// `failure_reason` so task queries surface it as the task `error`. Without
/// this the reason lives only in the command payload/event log and the task
/// projection reports `error: null` (silent degradation).
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
    let Some(reason) = reason else {
        return Ok(());
    };
    if !instance.data.is_object() {
        instance.data = json!({});
    }
    let data = instance
        .data
        .as_object_mut()
        .context("workflow instance data is not an object")?;
    data.insert("failure_reason".to_string(), json!(reason));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        RuntimeJobStatus, RuntimeKind, WorkflowCommand, WorkflowCommandType, WorkflowInstance,
        WorkflowRuntimeStore, WorkflowSubject,
    };
    use async_trait::async_trait;
    use harness_core::db::resolve_database_url;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct PreflightRuntimeExecutor {
        result: ActivityResult,
        executions: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl RuntimeJobExecutor for PreflightRuntimeExecutor {
        async fn preflight_result(&self, _job: &RuntimeJob) -> Option<ActivityResult> {
            Some(self.result.clone())
        }

        async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
            self.executions.fetch_add(1, Ordering::SeqCst);
            ActivityResult::succeeded("check", "executed")
        }
    }

    async fn enqueue_test_runtime_job(
        store: &WorkflowRuntimeStore,
        key: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: serde_json::Value,
    ) -> anyhow::Result<RuntimeJob> {
        let workflow = WorkflowInstance::new(
            "github_issue_pr",
            1,
            "implementing",
            WorkflowSubject::new("issue", format!("issue:{key}")),
        )
        .with_id(format!("runtime-worker-test-{key}"));
        store.upsert_instance(&workflow).await?;
        let activity = input
            .get("activity")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("test_activity");
        let command =
            WorkflowCommand::enqueue_activity(activity, format!("runtime-worker-test-{key}"));
        let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
        store
            .enqueue_runtime_job(&command_id, runtime_kind, runtime_profile, input)
            .await
    }

    #[tokio::test]
    async fn preflight_result_completes_job_before_runtime_turn_starts() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&harness_core::config::dirs::default_db_path(
            dir.path(),
            "workflow_runtime",
        ))
        .await?;
        let job = enqueue_test_runtime_job(
            &store,
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "check" }),
        )
        .await?;
        let executions = Arc::new(AtomicUsize::new(0));
        let executor = PreflightRuntimeExecutor {
            result: ActivityResult::cancelled("check", "Runtime worker disabled."),
            executions: executions.clone(),
        };

        let completed = RuntimeWorker::new(&store, "runtime-1")
            .with_lease_ttl(Duration::minutes(5))
            .run_once(&executor)
            .await?
            .ok_or_else(|| anyhow::anyhow!("worker should complete the claimed job"))?;

        assert_eq!(completed.id, job.id);
        assert_eq!(completed.status, RuntimeJobStatus::Cancelled);
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        let events = store.runtime_events_for(&completed.id).await?;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "RuntimeJobClaimed");
        assert_eq!(events[1].event_type, "ActivityResultReady");
        Ok(())
    }

    #[tokio::test]
    async fn runtime_worker_skips_remote_host_jobs_for_external_claims() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&harness_core::config::dirs::default_db_path(
            dir.path(),
            "workflow_runtime",
        ))
        .await?;
        let remote_job = enqueue_test_runtime_job(
            &store,
            "command-remote",
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({ "activity": "remote_check" }),
        )
        .await?;
        let local_job = enqueue_test_runtime_job(
            &store,
            "command-local",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "local_check" }),
        )
        .await?;
        let executions = Arc::new(AtomicUsize::new(0));
        let executor = PreflightRuntimeExecutor {
            result: ActivityResult::succeeded("local_check", "Local worker completed."),
            executions,
        };

        let completed = RuntimeWorker::new(&store, "runtime-1")
            .with_lease_ttl(Duration::minutes(5))
            .run_once(&executor)
            .await?
            .ok_or_else(|| anyhow::anyhow!("worker should claim the local job"))?;

        assert_eq!(completed.id, local_job.id);
        let remote = store
            .get_runtime_job(&remote_job.id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("remote job should remain pending for runtime host API")
            })?;
        assert_eq!(remote.status, RuntimeJobStatus::Pending);
        Ok(())
    }

    #[test]
    fn mark_failed_inline_command_persists_failure_reason_into_data() {
        use crate::runtime::WorkflowSubject;

        let mut instance = WorkflowInstance::new(
            "prompt_task",
            1,
            "implementing",
            WorkflowSubject::new("prompt", "1"),
        );
        let command = WorkflowCommand::new(
            WorkflowCommandType::MarkFailed,
            "runtime-completion:evt-1:failed",
            json!({ "reason": "Agent turn timed out after 900s" }),
        );

        apply_failure_reason_side_effect(&mut instance, &command).unwrap();

        assert_eq!(
            instance.data.get("failure_reason").and_then(Value::as_str),
            Some("Agent turn timed out after 900s"),
            "MarkFailed must surface its reason as the queryable failure_reason"
        );
    }

    #[test]
    fn mark_failed_without_reason_leaves_data_untouched() {
        use crate::runtime::WorkflowSubject;

        let mut instance = WorkflowInstance::new(
            "prompt_task",
            1,
            "implementing",
            WorkflowSubject::new("prompt", "1"),
        );
        let command = WorkflowCommand::new(
            WorkflowCommandType::MarkFailed,
            "runtime-completion:evt-2:failed",
            json!({}),
        );

        apply_failure_reason_side_effect(&mut instance, &command).unwrap();

        assert!(instance.data.get("failure_reason").is_none());
    }
}
