mod activity_contract;
mod activity_result;
mod child_workflow;
mod child_workflow_non_issue;
mod child_workflow_replay;
mod data_helpers;
mod executor;
mod pr_feedback_inspection;
mod prompt_input_telemetry;
mod prompt_packet;
mod runtime_profile;
mod workspace;

use crate::http::AppState;
use crate::runtime_projection::RuntimeWorkflowProjection;
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
    fn runtime_submission_completion_task_ignores_nonterminal_projection_status() {
        let instance = issue_instance("local_review_gate");

        assert!(runtime_submission_completion_task(&instance, None).is_none());
    }
}
