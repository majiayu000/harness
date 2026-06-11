mod activity_contract;
mod activity_result;
mod child_workflow;
mod data_helpers;
mod executor;
mod pr_feedback_inspection;
mod prompt_packet;
mod runtime_profile;
mod workspace;

use crate::http::AppState;
use crate::task_runner::{TaskFailureKind, TaskKind, TaskState, TaskStatus};
use chrono::Duration;
use data_helpers::{optional_data_string, optional_data_u64};
use executor::ServerRuntimeJobExecutor;
use harness_workflow::runtime::{
    ActivityErrorKind, ActivityResult, RuntimeJob, RuntimeJobStatus, RuntimeWorker,
    WorkflowInstance, GITHUB_ISSUE_PR_DEFINITION_ID, PROMPT_TASK_DEFINITION_ID,
};
use serde_json::Value;
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
    let worker = RuntimeWorker::new(store.as_ref(), owner).with_lease_ttl(lease_ttl);
    let executor = ServerRuntimeJobExecutor::new(state);
    let completed = worker.run_once(&executor).await?;
    if let Some(job) = completed.as_ref() {
        if let Err(error) = notify_runtime_submission_terminal(state, job).await {
            tracing::warn!(
                runtime_job_id = %job.id,
                "workflow runtime completion notification failed: {error}"
            );
        }
    }
    Ok(RuntimeJobWorkerTick::from_completed_job(completed))
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
    if !instance.is_terminal() {
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
    let status = task_status_for_runtime_state(&instance.state)?;
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

fn task_status_for_runtime_state(state: &str) -> Option<TaskStatus> {
    match state {
        "done" => Some(TaskStatus::Done),
        "failed" => Some(TaskStatus::Failed),
        "cancelled" => Some(TaskStatus::Cancelled),
        _ => None,
    }
}

fn runtime_job_activity_result(job: &RuntimeJob) -> Option<ActivityResult> {
    job.output
        .as_ref()
        .and_then(|output| serde_json::from_value(output.clone()).ok())
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
