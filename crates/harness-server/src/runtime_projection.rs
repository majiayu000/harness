use crate::task_runner::{
    SchedulerAuthorityState, TaskFailureKind, TaskId, TaskPhase, TaskSchedulerState, TaskStatus,
};
use harness_workflow::runtime::WorkflowInstance;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeActiveBucket {
    Running,
    Queued,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuntimeWorkflowProjection {
    pub(crate) task_status: TaskStatus,
    pub(crate) failure_kind: Option<TaskFailureKind>,
    pub(crate) phase: TaskPhase,
    pub(crate) scheduler: TaskSchedulerState,
    pub(crate) project_id: Option<String>,
    pub(crate) submission_handle: Option<TaskId>,
    pub(crate) legacy_dedupe_task_handle: Option<TaskId>,
}

impl RuntimeWorkflowProjection {
    pub(crate) fn from_workflow(workflow: &WorkflowInstance) -> Self {
        let task_status = workflow_state_to_task_status(&workflow.state);
        let scheduler = workflow_scheduler_state(&workflow.state, &task_status);
        Self {
            failure_kind: task_status.is_failure().then_some(TaskFailureKind::Task),
            phase: workflow_state_to_task_phase(&workflow.state),
            task_status,
            scheduler,
            project_id: runtime_string_field(&workflow.data, "project_id"),
            submission_handle: runtime_submission_handle(&workflow.data),
            legacy_dedupe_task_handle: legacy_dedupe_task_handle(&workflow.data),
        }
    }

    pub(crate) fn active_bucket(&self) -> Option<RuntimeActiveBucket> {
        if self.task_status.is_terminal() {
            return None;
        }
        match self.scheduler.authority_state {
            SchedulerAuthorityState::Running
            | SchedulerAuthorityState::Leased
            | SchedulerAuthorityState::Recovering => Some(RuntimeActiveBucket::Running),
            _ => Some(RuntimeActiveBucket::Queued),
        }
    }
}

pub(crate) fn runtime_string_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
}

pub(crate) fn workflow_state_to_task_status(state: &str) -> TaskStatus {
    match state {
        "awaiting_dependencies" => TaskStatus::AwaitingDeps,
        "scheduled" | "discovered" => TaskStatus::Pending,
        "planning" => TaskStatus::Planning,
        "implementing" | "replanning" | "addressing_feedback" => TaskStatus::Implementing,
        "pr_open"
        | "local_review_gate"
        | "awaiting_feedback"
        | "quality_gate_pending"
        | "ready_to_merge"
        | "blocked" => TaskStatus::Waiting,
        "done" | "passed" => TaskStatus::Done,
        "failed" => TaskStatus::Failed,
        "cancelled" => TaskStatus::Cancelled,
        _ => TaskStatus::Waiting,
    }
}

fn workflow_state_to_task_phase(state: &str) -> TaskPhase {
    match state {
        "done" | "passed" | "failed" | "cancelled" => TaskPhase::Terminal,
        "pr_open"
        | "local_review_gate"
        | "awaiting_feedback"
        | "quality_gate_pending"
        | "ready_to_merge" => TaskPhase::Review,
        "planning" | "replanning" | "blocked" => TaskPhase::Plan,
        _ => TaskPhase::Implement,
    }
}

fn workflow_scheduler_state(state: &str, status: &TaskStatus) -> TaskSchedulerState {
    match state {
        "awaiting_dependencies" => TaskSchedulerState::awaiting_dependencies(),
        "scheduled" | "discovered" => TaskSchedulerState::queued(),
        "planning" | "implementing" | "replanning" | "addressing_feedback" => TaskSchedulerState {
            authority_state: SchedulerAuthorityState::Running,
            owner: None,
            run_generation: 0,
            recovery_generation: 0,
            lease_expires_at: None,
        },
        "done" | "passed" | "failed" | "cancelled" => {
            let mut scheduler = TaskSchedulerState::queued();
            scheduler.mark_terminal(status);
            scheduler
        }
        "pr_open"
        | "local_review_gate"
        | "awaiting_feedback"
        | "quality_gate_pending"
        | "ready_to_merge"
        | "blocked" => TaskSchedulerState::queued(),
        _ => match status {
            TaskStatus::AwaitingDeps => TaskSchedulerState::awaiting_dependencies(),
            TaskStatus::Pending => TaskSchedulerState::queued(),
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled => {
                let mut scheduler = TaskSchedulerState::queued();
                scheduler.mark_terminal(status);
                scheduler
            }
            _ => TaskSchedulerState::queued(),
        },
    }
}

fn runtime_submission_handle(data: &serde_json::Value) -> Option<TaskId> {
    trimmed_string_field(data, "submission_id")
        .or_else(|| first_string_array_field(data, "task_ids"))
        .or_else(|| trimmed_string_field(data, "task_id"))
        .map(harness_core::types::TaskId)
}

fn legacy_dedupe_task_handle(data: &serde_json::Value) -> Option<TaskId> {
    trimmed_string_field(data, "task_id")
        .or_else(|| last_string_array_field(data, "task_ids"))
        .or_else(|| trimmed_string_field(data, "submission_id"))
        .map(harness_core::types::TaskId)
}

fn trimmed_string_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field).and_then(trimmed_string)
}

fn first_string_array_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(serde_json::Value::as_array)
        .and_then(|values| values.iter().find_map(trimmed_string))
}

fn last_string_array_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(serde_json::Value::as_array)
        .and_then(|values| values.iter().rev().find_map(trimmed_string))
}

fn trimmed_string(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{WorkflowInstance, WorkflowSubject};

    fn workflow(state: &str, data: serde_json::Value) -> WorkflowInstance {
        WorkflowInstance::new(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            state,
            WorkflowSubject::new("issue", "issue:1"),
        )
        .with_data(data)
    }

    #[test]
    fn projection_exposes_stable_submission_handle_and_current_legacy_handle() {
        let workflow = workflow(
            "implementing",
            serde_json::json!({
                "project_id": "/repo",
                "submission_id": "first-submission",
                "task_id": "current-task",
                "task_ids": ["first-submission", "previous-task"],
            }),
        );

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(
            projection.submission_handle.as_ref().map(TaskId::as_str),
            Some("first-submission")
        );
        assert_eq!(
            projection
                .legacy_dedupe_task_handle
                .as_ref()
                .map(TaskId::as_str),
            Some("current-task")
        );
    }

    #[test]
    fn projection_uses_task_history_when_explicit_task_id_is_missing() {
        let workflow = workflow(
            "implementing",
            serde_json::json!({
                "task_ids": ["first-submission", "current-task"],
            }),
        );

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(
            projection.submission_handle.as_ref().map(TaskId::as_str),
            Some("first-submission")
        );
        assert_eq!(
            projection
                .legacy_dedupe_task_handle
                .as_ref()
                .map(TaskId::as_str),
            Some("current-task")
        );
    }

    #[test]
    fn projection_marks_active_execution_states_as_running() {
        let workflow = workflow("implementing", serde_json::json!({}));

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(projection.task_status, TaskStatus::Implementing);
        assert_eq!(
            projection.scheduler.authority_state,
            SchedulerAuthorityState::Running
        );
        assert_eq!(
            projection.active_bucket(),
            Some(RuntimeActiveBucket::Running)
        );
    }

    #[test]
    fn projection_marks_review_wait_states_as_queued_active_work() {
        let workflow = workflow("awaiting_feedback", serde_json::json!({}));

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(projection.task_status, TaskStatus::Waiting);
        assert_eq!(
            projection.scheduler.authority_state,
            SchedulerAuthorityState::Queued
        );
        assert_eq!(
            projection.active_bucket(),
            Some(RuntimeActiveBucket::Queued)
        );
    }

    #[test]
    fn projection_excludes_terminal_work_from_active_counts() {
        let workflow = workflow("failed", serde_json::json!({}));

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(projection.task_status, TaskStatus::Failed);
        assert_eq!(
            projection.scheduler.authority_state,
            SchedulerAuthorityState::Failed
        );
        assert_eq!(projection.phase, TaskPhase::Terminal);
        assert_eq!(projection.active_bucket(), None);
    }
}
