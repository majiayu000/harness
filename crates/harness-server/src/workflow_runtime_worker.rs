mod activity_contract;
mod activity_result;
mod activity_status_contract;
mod child_workflow;
mod child_workflow_non_issue;
mod child_workflow_replay;
mod data_helpers;
mod executor;
mod merge_completion;
mod otel_trajectory;
mod pr_feedback_inspection;
mod prompt_input_telemetry;
mod prompt_packet;
mod repo_memory_prompt;
mod runtime_profile;
mod runtime_usage;
mod server_merge;
pub(crate) mod turn_engine;
mod workspace;

use crate::http::AppState;
use crate::runtime_circuit_breaker::{
    classify_agent_failure, CircuitBreakerEvent, CircuitBreakerEventKind, FailureClass,
};
use crate::runtime_projection::RuntimeWorkflowProjection;
use crate::task_runner::{TaskFailureKind, TaskKind, TaskState, TaskStatus};
use chrono::Duration;
use data_helpers::{optional_data_string, optional_data_u64};
use executor::ServerRuntimeJobExecutor;
use harness_core::types::{Decision, Event, SessionId};
use harness_workflow::runtime::{
    ActivityErrorKind, ActivityResult, RuntimeJob, RuntimeJobStatus, RuntimeWorker,
    WorkflowInstance, GITHUB_ISSUE_PR_DEFINITION_ID, PROMPT_TASK_DEFINITION_ID,
};
use otel_trajectory::emit_runtime_job_trajectory_completion;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeJobWorkerTick {
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub idle: bool,
}

impl RuntimeJobWorkerTick {
    fn from_completed_job(job: Option<RuntimeJob>) -> Self {
        match job.map(|job| job.status) {
            Some(RuntimeJobStatus::Succeeded) => Self {
                succeeded: 1,
                ..Self::default()
            },
            Some(RuntimeJobStatus::Failed) => Self {
                failed: 1,
                ..Self::default()
            },
            Some(RuntimeJobStatus::Cancelled) => Self {
                cancelled: 1,
                ..Self::default()
            },
            Some(RuntimeJobStatus::Pending | RuntimeJobStatus::Running) => Self::default(),
            None => Self {
                idle: true,
                ..Self::default()
            },
        }
    }

    pub(crate) fn touched_anything(&self) -> bool {
        self.succeeded > 0 || self.failed > 0 || self.cancelled > 0
    }
}

pub(crate) async fn run_runtime_job_worker_tick(
    state: &Arc<AppState>,
    owner: impl Into<String>,
    lease_ttl: Duration,
) -> anyhow::Result<RuntimeJobWorkerTick> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimeJobWorkerTick {
            idle: true,
            ..RuntimeJobWorkerTick::default()
        });
    };
    defer_open_runtime_profiles(state, store.as_ref()).await?;
    let worker = RuntimeWorker::new(store.as_ref(), owner)
        .with_lease_ttl(lease_ttl)
        .with_claim_guard(state.runtime_circuit_breakers.as_ref());
    let executor = ServerRuntimeJobExecutor::new(state);
    let mut completed = worker.run_once(&executor).await?;
    if let Some(job) = completed.as_mut() {
        if let Err(error) = emit_runtime_job_trajectory_completion(state, store.as_ref(), job).await
        {
            tracing::warn!(
                runtime_job_id = %job.id,
                "workflow runtime OTel trajectory emission failed: {error}"
            );
        }
        record_runtime_circuit_breaker_completion(state, store.as_ref(), job).await?;
        if let Err(error) = notify_runtime_submission_terminal(state, job).await {
            tracing::warn!(
                runtime_job_id = %job.id,
                "workflow runtime completion notification failed: {error}"
            );
        }
    }
    Ok(RuntimeJobWorkerTick::from_completed_job(completed))
}

async fn defer_open_runtime_profiles(
    state: &AppState,
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
) -> anyhow::Result<()> {
    for deferred in state
        .runtime_circuit_breakers
        .defer_open_profiles(chrono::Utc::now())
    {
        let deferred_jobs = store
            .defer_ready_runtime_jobs_for_profile(&deferred.profile, deferred.until)
            .await?;
        if deferred_jobs > 0 {
            tracing::warn!(
                runtime_profile = %deferred.profile,
                deferred_runtime_jobs = deferred_jobs,
                cooldown_until = %deferred.until,
                "runtime circuit breaker deferred ready jobs"
            );
        }
    }
    Ok(())
}

pub(crate) async fn record_runtime_circuit_breaker_completion(
    state: &AppState,
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    job: &mut RuntimeJob,
) -> anyhow::Result<()> {
    let events = match job.status {
        RuntimeJobStatus::Succeeded => state.runtime_circuit_breakers.record_success(
            &job.runtime_profile,
            &job.id,
            chrono::Utc::now(),
        ),
        RuntimeJobStatus::Failed => {
            let failure_class = runtime_job_failure_class(job);
            if let Some(updated) = store
                .record_runtime_job_failure_class(&job.id, failure_class.as_str())
                .await?
            {
                *job = updated;
            }
            state.runtime_circuit_breakers.record_failure(
                &job.runtime_profile,
                &job.id,
                failure_class,
                chrono::Utc::now(),
            )
        }
        RuntimeJobStatus::Cancelled | RuntimeJobStatus::Pending | RuntimeJobStatus::Running => {
            Vec::new()
        }
    };
    emit_circuit_breaker_events(state, events).await;
    Ok(())
}

pub(crate) async fn emit_circuit_breaker_events(
    state: &AppState,
    events: Vec<CircuitBreakerEvent>,
) {
    for event in events {
        emit_circuit_breaker_event(state, event).await;
    }
}

async fn emit_circuit_breaker_event(state: &AppState, event: CircuitBreakerEvent) {
    let class = event.class.map(FailureClass::as_str);
    let (level, breaker_state, decision, reason) = match event.kind {
        CircuitBreakerEventKind::Opened => (
            "error",
            "open",
            Decision::Block,
            "runtime circuit breaker opened",
        ),
        CircuitBreakerEventKind::Closed => (
            "info",
            "closed",
            Decision::Complete,
            "runtime circuit breaker closed",
        ),
        CircuitBreakerEventKind::Reset => (
            "info",
            "closed",
            Decision::Complete,
            "runtime circuit breaker reset",
        ),
    };
    let detail = json!({
        "level": level,
        "profile": event.profile,
        "state": breaker_state,
        "failure_class": class,
        "consecutive": event.consecutive,
        "cooldown_until": event.cooldown_until,
    });
    match event.kind {
        CircuitBreakerEventKind::Opened => {
            tracing::error!(
                runtime_profile = %detail["profile"],
                failure_class = ?detail["failure_class"],
                consecutive = ?event.consecutive,
                cooldown_until = ?event.cooldown_until,
                "runtime circuit breaker opened"
            );
            // External alert (GH1582 B-020): dedup key is the profile scope,
            // so a flapping breaker is suppressed within the dedup window.
            state
                .observability
                .alerts
                .raise(crate::alerting::producers::circuit_breaker_open(
                    &event.profile,
                    &format!(
                        "failure_class={:?} consecutive={:?} cooldown_until={:?}",
                        class, event.consecutive, event.cooldown_until
                    ),
                ));
        }
        CircuitBreakerEventKind::Closed | CircuitBreakerEventKind::Reset => tracing::info!(
            runtime_profile = %detail["profile"],
            failure_class = ?detail["failure_class"],
            "runtime circuit breaker recovered"
        ),
    }
    let mut observe_event = Event::new(
        SessionId::new(),
        "runtime_circuit_breaker",
        detail["profile"].as_str().unwrap_or("runtime"),
        decision,
    );
    observe_event.reason = Some(reason.to_string());
    observe_event.detail = Some(detail.to_string());
    if let Err(error) = state.observability.events.log(&observe_event).await {
        tracing::warn!("failed to record runtime circuit breaker event: {error}");
    }
}

pub(crate) async fn notify_runtime_submission_terminal(
    state: &AppState,
    job: &RuntimeJob,
) -> anyhow::Result<bool> {
    let Some(workflow_id) = job.input.get("workflow_id").and_then(Value::as_str) else {
        return Ok(false);
    };
    let result = runtime_job_activity_result(job);
    notify_runtime_submission_terminal_workflow(state, workflow_id, result.as_ref()).await
}

pub(crate) async fn notify_runtime_submission_terminal_workflow(
    state: &AppState,
    workflow_id: &str,
    result: Option<&ActivityResult>,
) -> anyhow::Result<bool> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(false);
    };
    let Some(instance) = store.get_instance(workflow_id).await? else {
        return Ok(false);
    };
    crate::workflow_runtime_submission::remove_terminal_prompt_submission_payload(
        store.as_ref(),
        &instance,
    )
    .await?;
    if let Err(error) = workspace::cleanup_terminal_runtime_workspace(state, &instance).await {
        tracing::warn!(
            workflow_id = %workflow_id,
            "terminal runtime workspace cleanup failed: {error}"
        );
    }
    let Some(callback) = state.intake.completion_callback.as_ref() else {
        return Ok(false);
    };
    let Some(task) = runtime_submission_completion_task(&instance, result) else {
        return Ok(false);
    };
    callback(task).await;
    Ok(true)
}

fn runtime_submission_completion_task(
    instance: &WorkflowInstance,
    result: Option<&ActivityResult>,
) -> Option<TaskState> {
    let projection = RuntimeWorkflowProjection::from_workflow(instance);
    let status = projection.task_status;
    if !status.is_terminal() {
        return None;
    }
    let task_kind = match instance.definition_id.as_str() {
        GITHUB_ISSUE_PR_DEFINITION_ID => TaskKind::Issue,
        PROMPT_TASK_DEFINITION_ID => TaskKind::Prompt,
        _ => return None,
    };
    let source = optional_data_string(instance, "source")?;
    let external_id = optional_data_string(instance, "external_id")?;
    let task_id = crate::workflow_runtime_submission::runtime_issue_task_handle(instance)?;
    let mut task = TaskState::new(task_id);
    task.task_kind = task_kind;
    task.status = status.clone();
    task.failure_kind = runtime_failure_kind(&status, result);
    task.source = Some(source);
    task.external_id = Some(external_id);
    task.repo = optional_data_string(instance, "repo");
    task.issue = if task_kind == TaskKind::Issue {
        optional_data_u64(instance, "issue_number")
    } else {
        None
    };
    task.project_root = optional_data_string(instance, "project_id").map(PathBuf::from);
    task.pr_url = optional_data_string(instance, "last_pr_url");
    task.description = match task_kind {
        TaskKind::Issue => task.issue.map(|issue| format!("issue #{issue}")),
        TaskKind::Prompt => optional_data_string(instance, "prompt_summary")
            .or_else(|| Some("prompt task".to_string())),
        _ => None,
    };
    task.error = runtime_completion_error(&status, result);
    Some(task)
}

fn runtime_job_activity_result(job: &RuntimeJob) -> Option<ActivityResult> {
    job.output
        .as_ref()
        .and_then(|output| serde_json::from_value(output.clone()).ok())
}

fn runtime_job_failure_class(job: &RuntimeJob) -> FailureClass {
    if runtime_job_activity_result(job)
        .is_some_and(|result| result.error_kind == Some(ActivityErrorKind::SpawnFailure))
    {
        return FailureClass::ZeroOutputSpawnFailure;
    }
    classify_agent_failure(job.error.as_deref().unwrap_or_default())
}

fn runtime_failure_kind(
    status: &TaskStatus,
    result: Option<&ActivityResult>,
) -> Option<TaskFailureKind> {
    if !status.is_failure() {
        return None;
    }
    match result.and_then(|result| result.error_kind) {
        Some(
            ActivityErrorKind::Retryable
            | ActivityErrorKind::SpawnFailure
            | ActivityErrorKind::Timeout
            | ActivityErrorKind::ExternalDependency,
        ) => Some(TaskFailureKind::WorkspaceLifecycle),
        _ => Some(TaskFailureKind::Task),
    }
}

fn runtime_completion_error(
    status: &TaskStatus,
    result: Option<&ActivityResult>,
) -> Option<String> {
    if !status.is_failure() {
        return None;
    }
    result
        .and_then(|result| {
            result
                .error
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| (!result.summary.trim().is_empty()).then_some(result.summary.trim()))
        })
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::WorkflowSubject;
    use serde_json::json;

    fn issue_instance(state: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            state,
            WorkflowSubject::new("issue", "issue:42"),
        )
        .with_data(json!({
            "task_id": "runtime-task-42",
            "project_id": "/tmp/project",
            "repo": "owner/repo",
            "issue_number": 42,
            "source": "github",
            "external_id": "issue:42",
            "last_pr_url": "https://github.com/owner/repo/pull/7",
        }))
    }

    fn prompt_instance(state: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            PROMPT_TASK_DEFINITION_ID,
            1,
            state,
            WorkflowSubject::new("prompt", "manual:prompt:42"),
        )
        .with_data(json!({
            "task_id": "runtime-prompt-42",
            "project_id": "/tmp/project",
            "prompt_summary": "prompt task",
            "source": "dashboard",
            "external_id": "manual:prompt:42",
        }))
    }

    #[test]
    fn runtime_submission_completion_task_maps_done_via_projection() {
        let instance = issue_instance("done");
        let result = ActivityResult::succeeded("implement_issue", "Runtime job passed.");

        let Some(task) = runtime_submission_completion_task(&instance, Some(&result)) else {
            panic!("projected terminal runtime issue should map to an intake task");
        };

        assert_eq!(task.id.as_str(), "runtime-task-42");
        assert_eq!(task.task_kind, TaskKind::Issue);
        assert_eq!(task.status, TaskStatus::Done);
        assert_eq!(task.source.as_deref(), Some("github"));
        assert_eq!(task.external_id.as_deref(), Some("issue:42"));
        assert_eq!(task.repo.as_deref(), Some("owner/repo"));
        assert_eq!(task.issue, Some(42));
        assert_eq!(
            task.pr_url.as_deref(),
            Some("https://github.com/owner/repo/pull/7")
        );
        assert_eq!(task.description.as_deref(), Some("issue #42"));
    }

    #[test]
    fn runtime_submission_completion_task_preserves_issue_intake_identity() {
        let instance = issue_instance("cancelled");
        let result = ActivityResult::cancelled("implement_issue", "Runtime job was cancelled.");

        let Some(task) = runtime_submission_completion_task(&instance, Some(&result)) else {
            panic!("terminal runtime issue should map to an intake task");
        };

        assert_eq!(task.id.as_str(), "runtime-task-42");
        assert_eq!(task.task_kind, TaskKind::Issue);
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert_eq!(task.source.as_deref(), Some("github"));
        assert_eq!(task.external_id.as_deref(), Some("issue:42"));
        assert_eq!(task.repo.as_deref(), Some("owner/repo"));
        assert_eq!(task.issue, Some(42));
    }

    #[test]
    fn runtime_submission_completion_task_preserves_prompt_intake_identity() {
        let instance = prompt_instance("done");
        let result = ActivityResult::succeeded("implement_prompt", "Prompt task completed.");

        let Some(task) = runtime_submission_completion_task(&instance, Some(&result)) else {
            panic!("terminal runtime prompt should map to an intake task");
        };

        assert_eq!(task.id.as_str(), "runtime-prompt-42");
        assert_eq!(task.task_kind, TaskKind::Prompt);
        assert_eq!(task.status, TaskStatus::Done);
        assert_eq!(task.source.as_deref(), Some("dashboard"));
        assert_eq!(task.external_id.as_deref(), Some("manual:prompt:42"));
        assert_eq!(task.issue, None);
        assert_eq!(task.description.as_deref(), Some("prompt task"));
    }

    #[test]
    fn runtime_submission_completion_task_marks_retryable_failures_as_transient() {
        let instance = issue_instance("failed");
        let result = ActivityResult::failed(
            "implement_issue",
            "Runtime dependency failed.",
            "provider temporarily unavailable",
        )
        .with_error_kind(ActivityErrorKind::ExternalDependency);

        let Some(task) = runtime_submission_completion_task(&instance, Some(&result)) else {
            panic!("terminal runtime issue should map to an intake task");
        };

        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.failure_kind, Some(TaskFailureKind::WorkspaceLifecycle));
        assert_eq!(
            task.error.as_deref(),
            Some("provider temporarily unavailable")
        );
    }

    #[test]
    fn runtime_job_failure_class_uses_structured_spawn_failure() -> anyhow::Result<()> {
        let mut job = RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        );
        let result = ActivityResult::failed(
            "implement_issue",
            "Agent completed without assistant output.",
            "Agent completed without assistant output; treating as spawn failure.",
        )
        .with_error_kind(ActivityErrorKind::SpawnFailure);
        job.complete(&result)?;

        assert_eq!(
            runtime_job_failure_class(&job),
            FailureClass::ZeroOutputSpawnFailure
        );
        Ok(())
    }

    #[test]
    fn runtime_job_failure_class_falls_back_to_error_text() -> anyhow::Result<()> {
        let mut job = RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        );
        let result = ActivityResult::failed(
            "implement_issue",
            "Codex limit reached.",
            "codex exited with exit status: 1: stderr=[Reading additional input]",
        );
        job.complete(&result)?;

        assert_eq!(
            runtime_job_failure_class(&job),
            FailureClass::QuotaInteractiveWait
        );
        Ok(())
    }

    #[test]
    fn runtime_submission_completion_task_ignores_nonterminal_projection_status() {
        let instance = issue_instance("local_review_gate");

        assert!(runtime_submission_completion_task(&instance, None).is_none());
    }
}
